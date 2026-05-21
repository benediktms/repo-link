//! repo-link-daemon binary entry point.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use app_daemon::Daemon;
use application_sync::SyncService;
use application_workspace::{RepoBindingService, WorkspaceService};
use clap::Parser;
use infra_config::RepoLinkConfig;
use infra_filesystem::TokioFilesystemProbe;
use infra_github::GithubTaskProvider;
use infra_sqlite::{
    SqliteRepoBindingRepository, SqliteTaskRepository, SqliteWorkspaceRepository, open_from_path,
};

#[derive(Parser, Debug)]
#[command(name = "repo-link-daemon", version, about = "Background reconciler for repo-link")]
struct Args {
    /// Tick interval in seconds.
    #[arg(long, default_value_t = 60, env = "REPO_LINK_INTERVAL_SECS")]
    interval_secs: u64,

    /// When set, the daemon prunes worktree entries it just marked missing.
    #[arg(long)]
    prune: bool,

    /// Database path override (defaults to platform data dir).
    #[arg(long, env = "REPO_LINK_DB")]
    db: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut cfg = RepoLinkConfig::from_env()?;
    if let Some(db) = args.db {
        cfg = cfg.with_database_path(db);
    }
    if let Some(parent) = cfg.database_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let db = open_from_path(&cfg.database_path).await?;
    let workspaces_repo: Arc<dyn ports::WorkspaceRepository> =
        Arc::new(SqliteWorkspaceRepository::new(db.clone()));
    let bindings_repo: Arc<dyn ports::RepoBindingRepository> =
        Arc::new(SqliteRepoBindingRepository::new(db.clone()));
    let tasks_repo: Arc<dyn ports::TaskRepository> = Arc::new(SqliteTaskRepository::new(db));

    let workspaces = WorkspaceService::new(workspaces_repo.clone());
    let bindings = RepoBindingService::new(workspaces_repo, bindings_repo.clone());

    let probe: Arc<dyn ports::FilesystemProbe> = Arc::new(TokioFilesystemProbe::new());

    let sync = cfg.github_token.clone().map(|token| {
        let provider: Arc<dyn ports::RemoteTaskProvider> =
            Arc::new(GithubTaskProvider::new(token));
        SyncService::new(tasks_repo.clone(), bindings_repo, provider)
    });

    let daemon = Daemon::new(workspaces, bindings, tasks_repo, probe, sync).with_prune(args.prune);
    eprintln!(
        "[daemon] starting (interval={}s prune={} db={})",
        args.interval_secs,
        args.prune,
        cfg.database_path.display()
    );
    daemon.run(Duration::from_secs(args.interval_secs)).await?;
    Ok(())
}
