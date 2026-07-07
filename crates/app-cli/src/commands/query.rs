//! `rl query` dispatch — the read-only workspace views.

use anyhow::{Result, anyhow};
use infra_config::RepoLinkConfig;

use crate::cli::{QueryCmd, WorkspaceArg};
use crate::commands::repo::resolve_workspace;
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
            let workspace = resolve_workspace(svc, workspace).await?;
            let v = svc.query.overview(&workspace).await?;
            render::overview(&v);
        }
        QueryCmd::Blocked {
            ws: WorkspaceArg { workspace },
        } => {
            let workspace = resolve_workspace(svc, workspace).await?;
            let v = svc.query.blocked_tasks(&workspace).await?;
            render::blocked(&v);
        }
        QueryCmd::Stale {
            ws: WorkspaceArg { workspace },
        } => {
            let workspace = resolve_workspace(svc, workspace).await?;
            let v = svc.query.stale_worktrees(&workspace).await?;
            render::stale(&v);
        }
        QueryCmd::Unsynced {
            ws: WorkspaceArg { workspace },
        } => {
            let workspace = resolve_workspace(svc, workspace).await?;
            let v = svc.query.unsynced_tasks(&workspace).await?;
            render::unsynced(&v);
        }
        QueryCmd::Contributors {
            ws: WorkspaceArg { workspace },
        } => {
            let workspace = resolve_workspace(svc, workspace).await?;
            let v = svc.query.contributors(&workspace).await?;
            render::contributors(&v);
        }
        QueryCmd::Drift {
            ws: WorkspaceArg { workspace },
        } => {
            let workspace = resolve_workspace(svc, workspace).await?;
            let v = svc.query.drift(&workspace).await?;
            render::drift(&v);
        }
        QueryCmd::Ready {
            ws: WorkspaceArg { workspace },
        } => {
            let workspace = resolve_workspace(svc, workspace).await?;
            let v = svc.query.ready_tasks(&workspace).await?;
            render::ready(&v);
        }
        QueryCmd::Mine {
            ws: WorkspaceArg { workspace },
            assignee,
        } => {
            // Resolution chain — see `resolve_mine_assignee`. The cached
            // GitHub login comes ahead of the git committer identity so a
            // bare `query mine` round-trips with `task claim` (which assigns
            // that login); git user.name / env vars remain as fallbacks. A
            // token-file permission error degrades to "no login" rather than
            // blocking a read-only view.
            let assignee = resolve_mine_assignee(
                assignee,
                cfg.resolve_github_login().ok().flatten(),
                git_user_name(),
                std::env::var("REPO_LINK_USER").ok(),
                std::env::var("USER").ok(),
            )
            .ok_or_else(|| {
                anyhow!(
                    "no assignee: pass --assignee, run `rl gh auth`, configure \
                     `git config user.name`, or set REPO_LINK_USER / USER"
                )
            })?;
            let workspace = resolve_workspace(svc, workspace).await?;
            let v = svc.query.assigned_to(&workspace, &assignee).await?;
            render::assigned(&v);
        }
        QueryCmd::Children { id } => {
            // Friendly-ID resolution lives in TaskService; the query layer is
            // UUID-only, so resolve here before handing it the canonical id.
            let parent_uuid = svc.tasks.resolve_id(&id).await?;
            let v = svc.query.children(&parent_uuid).await?;
            render::children(&v);
        }
    }
    Ok(())
}

/// Resolve the effective `query mine` assignee from the precedence chain,
/// rejecting empty / whitespace-only candidates at every step:
///
/// `--assignee` > cached GitHub login > `git config user.name` >
/// `REPO_LINK_USER` > `$USER`.
///
/// The GitHub login precedes the git committer identity so a bare
/// `query mine` round-trips with `task claim` (which assigns that login).
/// Pure over its inputs so the precedence is unit-testable without touching
/// env vars, git, or the token file. Returns `None` when no source yields a
/// non-empty value.
fn resolve_mine_assignee(
    explicit: Option<String>,
    github_login: Option<String>,
    git_user: Option<String>,
    repo_link_user: Option<String>,
    unix_user: Option<String>,
) -> Option<String> {
    [explicit, github_login, git_user, repo_link_user, unix_user]
        .into_iter()
        .flatten()
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::resolve_mine_assignee;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn explicit_assignee_wins() {
        let got = resolve_mine_assignee(s("explicit"), s("login"), s("git"), s("env"), s("user"));
        assert_eq!(got.as_deref(), Some("explicit"));
    }

    #[test]
    fn github_login_precedes_git_user_so_claim_round_trips() {
        // The whole point of #84: `claim` assigns the GitHub login, so a bare
        // `mine` must prefer it over the (display-name) git committer identity.
        let got = resolve_mine_assignee(
            None,
            s("benediktms"),
            s("Benedikt Schnatterbeck"),
            None,
            None,
        );
        assert_eq!(got.as_deref(), Some("benediktms"));
    }

    #[test]
    fn falls_through_to_git_user_when_no_login() {
        let got = resolve_mine_assignee(None, None, s("git-user"), s("env"), s("user"));
        assert_eq!(got.as_deref(), Some("git-user"));
    }

    #[test]
    fn repo_link_user_precedes_unix_user() {
        let got = resolve_mine_assignee(None, None, None, s("env"), s("user"));
        assert_eq!(got.as_deref(), Some("env"));
    }

    #[test]
    fn empty_explicit_falls_through_instead_of_short_circuiting() {
        let got = resolve_mine_assignee(s(""), None, s("git-user"), None, None);
        assert_eq!(got.as_deref(), Some("git-user"));
    }

    #[test]
    fn empty_and_whitespace_env_values_are_rejected() {
        // REPO_LINK_USER="" / USER="" must not resolve to an empty assignee.
        let got = resolve_mine_assignee(None, None, None, s(""), s("   "));
        assert_eq!(got, None);
    }

    #[test]
    fn whitespace_is_trimmed_from_the_winner() {
        let got = resolve_mine_assignee(s("  spaced  "), None, None, None, None);
        assert_eq!(got.as_deref(), Some("spaced"));
    }

    #[test]
    fn all_empty_yields_none() {
        let got = resolve_mine_assignee(None, None, None, None, None);
        assert_eq!(got, None);
    }
}
