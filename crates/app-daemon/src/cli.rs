use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use application_sync::{OutboxDrainer, ProjectPoller};
use application_workspace::{RepoBindingService, WorkspaceService};
use clap::Parser;
use infra_config::RepoLinkConfig;
use infra_filesystem::TokioFilesystemProbe;
use infra_github::GithubAdapter;
use infra_sqlite::{
    SqliteOutboxRepository, SqliteProjectRepository, SqliteRepoBindingRepository,
    SqliteTaskRepository, SqliteWorkspaceRepository, open_from_path,
};
use tracing::info;

use crate::daemon::Daemon;
use crate::logging::LogFormat;

#[derive(Parser, Debug)]
#[command(name = "rld", version, about = "Background reconciler for repo-link")]
pub struct Args {
    /// Poller-task tick interval in seconds: project poll, worktree reconcile,
    /// grace-prune, and heartbeat share this cadence. Defaults to
    /// `PROJECT_POLLER_INTERVAL` (the Stage-7 constant); override for slower or
    /// faster reconcile. The drainer task's periodic sweep is separate and
    /// fixed at `OUTBOX_DRAINER_PERIODIC_SWEEP`.
    ///
    /// Must be `>= 1`: `run_poller_task` builds a `tokio::time::interval`, which
    /// panics on a zero period. The `range(1..)` parser rejects `0` at clap
    /// parse time — and clap applies the same parser to the
    /// `REPO_LINK_INTERVAL_SECS` env value, so neither the flag nor the env can
    /// smuggle a zero through.
    #[arg(long, default_value_t = crate::daemon::PROJECT_POLLER_INTERVAL.as_secs(), value_parser = clap::value_parser!(u64).range(1..), env = "REPO_LINK_INTERVAL_SECS")]
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
    /// `--interval-secs × --missing-grace-ticks` (defaults: 45s × 3 ≈ 2¼
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
    let tasks_repo: Arc<dyn ports::TaskRepository> =
        Arc::new(SqliteTaskRepository::new(db.clone()));
    let projects_repo: Arc<dyn ports::ProjectRepository> =
        Arc::new(SqliteProjectRepository::new(db.clone()));
    let outbox_repo: Arc<dyn ports::OutboxRepository> = Arc::new(SqliteOutboxRepository::new(db));

    let workspaces = WorkspaceService::new(workspaces_repo.clone());
    let bindings = RepoBindingService::new(workspaces_repo.clone(), bindings_repo.clone());

    let probe: Arc<dyn ports::FilesystemProbe> = Arc::new(TokioFilesystemProbe::new());

    // The drainer (outbound) and poller (inbound) are both gated on a resolved
    // GitHub token the same way `SyncService` used to be — no token, no network
    // work, but worktree reconciliation still runs. Resolution goes through
    // `RepoLinkConfig::resolve_github_token()` (the same source `app-cli` uses),
    // so a token written only to the token file by `rl gh auth` — i.e. not set
    // via env/inline `github_token` — still enables the daemon's network tasks
    // rather than silently disabling them. An insecure-permissions / I/O error
    // on the token file propagates verbatim rather than being treated as "no
    // token". The GithubAdapter implements both RemoteTaskProvider (issues) and
    // RemoteProjectProvider (Projects v2), so one adapter backs both. Built via
    // the shared base-URL-aware constructor so REPO_LINK_GITHUB_API_BASE_URL is
    // honoured exactly as in app-cli (#100 — the daemon previously called `new`
    // and dropped the override).
    let (drainer, poller, outbox) = match cfg.resolve_github_token()? {
        Some(token) => {
            let adapter = Arc::new(GithubAdapter::from_env_parts(
                token,
                cfg.github_api_base_url.as_deref(),
            )?);
            let remote_tasks: Arc<dyn ports::RemoteTaskProvider> = adapter.clone();
            let remote_projects: Arc<dyn ports::RemoteProjectProvider> = adapter;
            let drainer = Arc::new(OutboxDrainer::new(
                outbox_repo.clone(),
                tasks_repo.clone(),
                workspaces_repo.clone(),
                projects_repo.clone(),
                remote_tasks,
                remote_projects.clone(),
            ));
            let poller = Arc::new(ProjectPoller::new(
                projects_repo,
                tasks_repo.clone(),
                remote_projects,
            ));
            (Some(drainer), Some(poller), Some(outbox_repo))
        }
        None => (None, None, None),
    };

    let daemon = Daemon::new(
        workspaces, bindings, tasks_repo, probe, drainer, poller, outbox,
    )
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
    // `run` drives two `tokio::spawn`'d tasks off `Arc<Self>` (Stage 7, #55),
    // so the daemon must be shared.
    Arc::new(daemon)
        .run(Duration::from_secs(args.interval_secs))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--interval-secs 0` is rejected at clap parse time: a zero period would
    /// panic `tokio::time::interval` in `run_poller_task`. The `range(1..)`
    /// parser is what guards it (and, since clap applies the same parser to the
    /// `REPO_LINK_INTERVAL_SECS` env value, that path is guarded too).
    #[test]
    fn rejects_zero_interval_secs() {
        let err =
            Args::try_parse_from(["rld", "--interval-secs", "0"]).expect_err("0 must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    /// A positive interval parses and round-trips unchanged.
    #[test]
    fn accepts_positive_interval_secs() {
        let args =
            Args::try_parse_from(["rld", "--interval-secs", "1"]).expect("1 is a valid interval");
        assert_eq!(args.interval_secs, 1);
    }
}
