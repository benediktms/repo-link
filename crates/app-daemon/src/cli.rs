use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use application_sync::SyncService;
use application_workspace::{RepoBindingService, WorkspaceService};
use clap::Parser;
use infra_config::RepoLinkConfig;
use infra_filesystem::TokioFilesystemProbe;
use infra_github::GithubTaskProvider;
use infra_sqlite::{
    SqliteRepoBindingRepository, SqliteTaskRepository, SqliteWorkspaceRepository, open_from_path,
};
use tracing::info;

use crate::daemon::Daemon;
use crate::logging::LogFormat;

#[derive(Parser, Debug)]
#[command(name = "rld", version, about = "Background reconciler for repo-link")]
pub struct Args {
    /// Tick interval in seconds.
    #[arg(long, default_value_t = 60, env = "REPO_LINK_INTERVAL_SECS")]
    pub interval_secs: u64,

    /// When set, the daemon drops worktree entries whose paths have stayed
    /// missing across `--missing-grace-ticks` consecutive ticks (default 3).
    /// Without this flag, missing paths are marked `MissingPath` on the
    /// binding but never removed.
    #[arg(long)]
    pub prune: bool,

    /// Database path override (defaults to platform data dir).
    #[arg(long, env = "REPO_LINK_DB")]
    pub db: Option<PathBuf>,

    /// Log output format. `pretty` writes ANSI-coloured text to stdout
    /// for foreground/dev runs; `json` writes a daily-rotated
    /// `daemon.log` next to the database for use under launchd/systemd.
    #[arg(long, value_enum, default_value_t = LogFormat::Pretty)]
    pub log_format: LogFormat,

    /// Number of consecutive ticks a worktree path must be missing before
    /// `--prune` actually drops it. Wall-clock grace =
    /// `--interval-secs × --missing-grace-ticks` (defaults: 60s × 3 = 3
    /// minutes). The counter is process-local — restarting the daemon
    /// resets it to zero, so short-lived runs cannot trigger a
    /// grace-protected prune.
    #[arg(long, default_value_t = 3, env = "REPO_LINK_MISSING_GRACE_TICKS")]
    pub missing_grace_ticks: u32,
}

/// Library entrypoint called from the `rld` bin shim.
pub async fn run_cli() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut cfg = RepoLinkConfig::from_env()?;
    if let Some(db) = args.db {
        cfg = cfg.with_database_path(db);
    }
    if let Some(parent) = cfg.database_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    // Logs live next to the SQLite db so all daemon state co-locates under
    // the platform data dir. The guard keeps the non-blocking file-write
    // worker alive; dropping it at process exit flushes any buffered events.
    let log_dir = cfg
        .database_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let _log_guard = crate::logging::init_subscriber(args.log_format, &log_dir);

    let db = open_from_path(&cfg.database_path).await?;
    let workspaces_repo: Arc<dyn ports::WorkspaceRepository> =
        Arc::new(SqliteWorkspaceRepository::new(db.clone()));
    let bindings_repo: Arc<dyn ports::RepoBindingRepository> =
        Arc::new(SqliteRepoBindingRepository::new(db.clone()));
    let tasks_repo: Arc<dyn ports::TaskRepository> = Arc::new(SqliteTaskRepository::new(db));

    let workspaces = WorkspaceService::new(workspaces_repo.clone());
    let bindings = RepoBindingService::new(workspaces_repo, bindings_repo.clone());

    let probe: Arc<dyn ports::FilesystemProbe> = Arc::new(TokioFilesystemProbe::new());

    let sync = match cfg.github_token.clone() {
        Some(token) => {
            let provider: Arc<dyn ports::RemoteTaskProvider> =
                Arc::new(GithubTaskProvider::new(token)?);
            Some(SyncService::new(
                tasks_repo.clone(),
                bindings_repo,
                provider,
            ))
        }
        None => None,
    };

    let daemon = Daemon::new(workspaces, bindings, tasks_repo, probe, sync)
        .with_prune(args.prune)
        .with_missing_grace_ticks(args.missing_grace_ticks)
        // Co-locate last_tick.json with the db + log so a `--db` override
        // relocates the whole daemon state consistently.
        .with_state_dir(log_dir.clone())
        .with_interval_secs(args.interval_secs);
    // Read the coerced value back from the daemon so a run with
    // `REPO_LINK_MISSING_GRACE_TICKS=0` logs the effective `1` rather
    // than the raw input. Single source of truth lives in
    // `with_missing_grace_ticks`.
    info!(
        interval_secs = args.interval_secs,
        prune = args.prune,
        missing_grace_ticks = daemon.missing_grace_ticks(),
        db = %cfg.database_path.display(),
        log_format = ?args.log_format,
        "daemon starting"
    );
    daemon.run(Duration::from_secs(args.interval_secs)).await?;
    Ok(())
}
