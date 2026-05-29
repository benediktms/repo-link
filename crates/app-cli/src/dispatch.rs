//! The library entrypoint (`run`) and the central command match
//! (`dispatch`). `run` parses the CLI, loads config, bootstraps services,
//! then hands off to `dispatch`, which fans each `Cmd` variant out to the
//! per-area handlers in [`crate::commands`].

use anyhow::Result;
use infra_config::RepoLinkConfig;

use crate::cli::{Cli, Cmd};
use crate::commands::agents::agents_dispatch;
use crate::commands::gh::gh_dispatch;
use crate::commands::project::project_dispatch;
use crate::commands::query::query_dispatch;
use crate::commands::repo::repo_dispatch;
use crate::commands::repo::worktree_dispatch;
use crate::commands::sync::sync_dispatch;
use crate::commands::task::task_dispatch;
use crate::commands::workspace::workspace_dispatch;
use crate::daemon;
use crate::services::{Services, bootstrap};
use clap::Parser;

/// Library entrypoint shared by both `repo-link` and `rl` bin shims.
pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = RepoLinkConfig::from_env()?;
    if let Some(db) = cli.db.clone() {
        cfg = cfg.with_database_path(db);
    }
    if let Some(parent) = cfg.database_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let services = bootstrap(&cfg).await?;
    dispatch(cli, &services, &cfg).await
}

pub(crate) async fn dispatch(cli: Cli, svc: &Services, cfg: &RepoLinkConfig) -> Result<()> {
    match cli.cmd {
        Cmd::Workspace(c) => workspace_dispatch(c, svc).await,
        Cmd::Repo(c) => repo_dispatch(c, svc).await,
        Cmd::Worktree(c) => worktree_dispatch(c, svc).await,
        Cmd::Task(c) => task_dispatch(c, svc, cfg).await,
        Cmd::Query(c) => query_dispatch(c, svc, cfg).await,
        Cmd::Sync(c) => sync_dispatch(c, svc, cfg).await,
        Cmd::Gh(c) => gh_dispatch(c, cfg).await,
        Cmd::Agents(c) => agents_dispatch(c, svc).await,
        Cmd::Project(c) => project_dispatch(c, svc, cfg).await,
        Cmd::Daemon(c) => daemon::dispatch(c, cfg).await,
    }
}
