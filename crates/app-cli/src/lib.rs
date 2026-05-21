//! repo-link CLI — also installed as `rl`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use application_query::QueryService;
use application_sync::SyncService;
use application_task::TaskService;
use application_workspace::{RepoBindingService, WorkspaceService};
use clap::{Parser, Subcommand};
use dto_shared::{
    AddTaskRelationCmd, AttachRepoCmd, CreateTaskCmd, CreateWorkspaceCmd, LinkWorktreeCmd,
    ListTasksQuery, ListWorkspacesQuery, LocateMatchDto, LocateResponseDto, UnlinkWorktreeCmd,
};
use infra_config::RepoLinkConfig;
use infra_filesystem::{TokioFilesystemProbe, discover_repos_under};
use infra_git::discover_canonical;
use infra_github::GithubTaskProvider;
use infra_sqlite::{
    SqliteRepoBindingRepository, SqliteTaskRepository, SqliteTaskSnapshotRepository,
    SqliteWorkspaceRepository, open_from_path,
};

mod render;

#[derive(Parser, Debug)]
#[command(
    name = "repo-link",
    version,
    about = "Local-first workspace + task manager. All output is JSON; pipe through `jq` for human-friendly views."
)]
struct Cli {
    /// SQLite database path. Falls back to platform data dir.
    #[arg(long, env = "REPO_LINK_DB", global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Workspace lifecycle.
    #[command(subcommand)]
    Workspace(WorkspaceCmd),
    /// Repo attachment + bindings.
    #[command(subcommand)]
    Repo(RepoCmd),
    /// Worktree path links.
    #[command(subcommand)]
    Worktree(WorktreeCmd),
    /// Task drafts and lifecycle.
    #[command(subcommand)]
    Task(TaskCmd),
    /// Read-only workspace views.
    #[command(subcommand)]
    Query(QueryCmd),
    /// Promote / push / pull tasks against GitHub.
    #[command(subcommand)]
    Sync(SyncCmd),
    /// GitHub helper commands.
    #[command(subcommand)]
    Gh(GhCmd),
}

#[derive(Subcommand, Debug)]
enum WorkspaceCmd {
    Create {
        name: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        local_only: bool,
    },
    List {
        #[arg(long)]
        include_archived: bool,
    },
    Show {
        id: String,
    },
    Activate {
        id: String,
    },
    Pause {
        id: String,
    },
    Archive {
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum RepoCmd {
    Attach {
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        url: String,
        #[arg(long)]
        canonical: String,
        #[arg(long)]
        branch: Option<String>,
        /// Local checkout to register as a worktree of this binding.
        /// Defaults to the current working directory. The path's git
        /// origin must canonicalise to `--canonical`; otherwise the
        /// command errors.
        ///
        /// When the same repo is cloned to multiple folders on disk
        /// (separate `.git` dirs rather than `git worktree`-linked
        /// checkouts), call `attach` once per path with `--path`;
        /// each call merges into the same binding and accumulates
        /// another worktree entry.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Skip auto-linking the current path. Use this when you're
        /// sitting in one clone but don't want it recorded under this
        /// binding — e.g. you have the same repo cloned twice and
        /// only the *other* clone should be the tracked checkout.
        /// Combine with `--path <other-clone>` (or follow up with
        /// `rl worktree link`) to register the intended path instead.
        #[arg(long)]
        no_link: bool,
    },
    Detach {
        id: String,
    },
    List {
        #[arg(long)]
        workspace: String,
    },
    Show {
        id: String,
    },
    /// Walk a directory and report every git repo found, with its origin URL.
    /// Use this to populate a workspace from `~/code/` in one shot.
    Discover {
        #[arg(long)]
        path: PathBuf,
    },
    /// Discover which repo binding (if any) owns the given path.
    /// Reads the path's git origin, canonicalises it, and looks for a
    /// matching binding across all non-archived workspaces.
    Locate {
        /// Path to probe. Defaults to current working directory.
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum WorktreeCmd {
    Link {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        path: String,
        #[arg(long)]
        branch: Option<String>,
    },
    Unlink {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        path: String,
    },
    PruneMissing {
        #[arg(long)]
        repo: String,
    },
    /// Scan every worktree in a workspace, mark missing paths, optionally
    /// drop them. Use this after switching machines or pruning checkouts.
    Reconcile {
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        prune: bool,
    },
}

#[derive(Subcommand, Debug)]
enum TaskCmd {
    Create {
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        priority: Option<String>,
    },
    Show {
        id: String,
    },
    List {
        #[arg(long)]
        workspace: Option<String>,
        /// Filter by lifecycle status (`open` / `in_progress` / `blocked` / `done` / `archived`).
        #[arg(long)]
        status: Option<String>,
        /// Filter by sync state (`local_only` / `staged` / `synced` / `dirty_local` / `dirty_remote` / `conflict`).
        #[arg(long)]
        sync_state: Option<String>,
        #[arg(long)]
        include_archived: bool,
    },
    /// Stage one or more tasks for sync.
    Stage {
        #[arg(required = true)]
        tasks: Vec<String>,
    },
    /// Start work on one or more tasks (Open|Blocked → InProgress).
    Start {
        #[arg(required = true)]
        tasks: Vec<String>,
    },
    /// Mark one or more tasks complete.
    Complete {
        #[arg(required = true)]
        tasks: Vec<String>,
    },
    /// Reopen one or more `Done` tasks back to `Open`.
    Reopen {
        #[arg(required = true)]
        tasks: Vec<String>,
    },
    /// Move one or more tasks to `Blocked`.
    Block {
        #[arg(required = true)]
        tasks: Vec<String>,
    },
    /// Move one or more `Blocked` tasks back to `Open`.
    Unblock {
        #[arg(required = true)]
        tasks: Vec<String>,
    },
    /// Archive one or more tasks.
    Archive {
        #[arg(required = true)]
        tasks: Vec<String>,
    },
    Relate {
        id: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        other: String,
    },
    /// List the full snapshot history for a task.
    Snapshots {
        id: String,
    },
    /// Roll a task back to a historical snapshot version.
    Rollback {
        id: String,
        #[arg(long)]
        to_version: u64,
    },
}

#[derive(Subcommand, Debug)]
enum QueryCmd {
    Overview {
        #[arg(long)]
        workspace: String,
    },
    Blocked {
        #[arg(long)]
        workspace: String,
    },
    Stale {
        #[arg(long)]
        workspace: String,
    },
    Unsynced {
        #[arg(long)]
        workspace: String,
    },
    Contributors {
        #[arg(long)]
        workspace: String,
    },
    Drift {
        #[arg(long)]
        workspace: String,
    },
    /// Tasks that are actionable now: open + not transitively blocked.
    Ready {
        #[arg(long)]
        workspace: String,
    },
    /// Open tasks assigned to a user. Defaults to $REPO_LINK_USER or $USER.
    Mine {
        #[arg(long)]
        workspace: String,
        #[arg(long, env = "REPO_LINK_USER")]
        assignee: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum SyncCmd {
    /// Create the remote issue for a Draft/Staged task.
    Promote {
        #[arg(long)]
        task: String,
    },
    /// Push local edits (state = DirtyLocal) to the remote.
    Push {
        #[arg(long)]
        task: String,
    },
    /// Pull the latest remote snapshot and reconcile.
    Pull {
        #[arg(long)]
        task: String,
    },
}

#[derive(Subcommand, Debug)]
enum GhCmd {
    /// Save the GitHub token to a permission-restricted config file.
    Auth {
        /// Token value. If omitted, prompts on stdin with echo disabled.
        /// Passing it as a flag avoids stdin but leaves the value in shell history.
        #[arg(long)]
        token: Option<String>,
        /// Skip the overwrite confirmation if the file already exists.
        #[arg(long)]
        force: bool,
    },
}

struct Services {
    workspaces: WorkspaceService,
    bindings: RepoBindingService,
    tasks: TaskService,
    query: QueryService,
    tasks_repo: Arc<dyn ports::TaskRepository>,
    bindings_repo: Arc<dyn ports::RepoBindingRepository>,
}

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

async fn bootstrap(cfg: &RepoLinkConfig) -> Result<Services> {
    let db = open_from_path(&cfg.database_path).await?;
    let workspaces_repo: Arc<dyn ports::WorkspaceRepository> =
        Arc::new(SqliteWorkspaceRepository::new(db.clone()));
    let bindings_repo: Arc<dyn ports::RepoBindingRepository> =
        Arc::new(SqliteRepoBindingRepository::new(db.clone()));
    let tasks_repo: Arc<dyn ports::TaskRepository> =
        Arc::new(SqliteTaskRepository::new(db.clone()));
    let snapshots_repo: Arc<dyn ports::TaskSnapshotRepository> =
        Arc::new(SqliteTaskSnapshotRepository::new(db));

    Ok(Services {
        workspaces: WorkspaceService::new(workspaces_repo.clone()),
        bindings: RepoBindingService::new(workspaces_repo.clone(), bindings_repo.clone()),
        tasks: TaskService::new(tasks_repo.clone(), snapshots_repo),
        query: QueryService::new(workspaces_repo, bindings_repo.clone(), tasks_repo.clone()),
        tasks_repo,
        bindings_repo,
    })
}

async fn dispatch(cli: Cli, svc: &Services, cfg: &RepoLinkConfig) -> Result<()> {
    match cli.cmd {
        Cmd::Workspace(c) => workspace_dispatch(c, svc).await,
        Cmd::Repo(c) => repo_dispatch(c, svc).await,
        Cmd::Worktree(c) => worktree_dispatch(c, svc).await,
        Cmd::Task(c) => task_dispatch(c, svc).await,
        Cmd::Query(c) => query_dispatch(c, svc, cfg).await,
        Cmd::Sync(c) => sync_dispatch(c, svc, cfg).await,
        Cmd::Gh(c) => gh_dispatch(c, cfg),
    }
}

async fn sync_dispatch(cmd: SyncCmd, svc: &Services, cfg: &RepoLinkConfig) -> Result<()> {
    let token = cfg
        .resolve_github_token()
        .map_err(|e| anyhow!("{e}"))?
        .ok_or_else(|| {
            anyhow!(
                "sync requires REPO_LINK_GITHUB_TOKEN or GITHUB_TOKEN to be set, \
                 or a token file at {} (write one with `rl gh auth`)",
                cfg.token_file_path.display()
            )
        })?;
    let provider: Arc<dyn ports::RemoteTaskProvider> =
        Arc::new(GithubTaskProvider::new(token));
    let sync = SyncService::new(svc.tasks_repo.clone(), svc.bindings_repo.clone(), provider);
    let summary = match cmd {
        SyncCmd::Promote { task } => sync.promote(&task).await?,
        SyncCmd::Push { task } => sync.push(&task).await?,
        SyncCmd::Pull { task } => sync.pull(&task).await?,
    };
    render::sync(&summary);
    Ok(())
}

fn gh_dispatch(cmd: GhCmd, cfg: &RepoLinkConfig) -> Result<()> {
    match cmd {
        GhCmd::Auth { token, force } => gh_auth(token, force, cfg),
    }
}

fn gh_auth(token: Option<String>, force: bool, cfg: &RepoLinkConfig) -> Result<()> {
    // Guard against overwriting an existing token file without explicit consent.
    if cfg.token_file_path.exists() && !force {
        eprint!(
            "token file {} already exists. Overwrite? [y/N]: ",
            cfg.token_file_path.display()
        );
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| anyhow!("failed to read confirmation: {e}"))?;
        let answer = line.trim().to_lowercase();
        if answer != "y" && answer != "yes" {
            return Err(anyhow!("aborted; pass --force to overwrite"));
        }
    }

    // Resolve the token: explicit --token wins, then a best-effort fetch from
    // the official `gh` CLI (so `gh auth login` users don't need to copy a
    // PAT by hand), and finally fall back to a hidden interactive prompt.
    let raw_token = match token {
        Some(t) => t,
        None => match try_gh_cli_token() {
            Some(t) => {
                eprintln!("note: using token from `gh auth token`.");
                t
            }
            None => rpassword::prompt_password("Paste GitHub token (input hidden): ")
                .map_err(|e| anyhow!("failed to read token: {e}"))?,
        },
    };
    let trimmed = raw_token.trim().to_string();
    if trimmed.is_empty() {
        return Err(anyhow!("token must not be empty"));
    }

    write_token_file(&cfg.token_file_path, &trimmed)?;

    let path_str = cfg
        .token_file_path
        .canonicalize()
        .unwrap_or_else(|_| cfg.token_file_path.clone())
        .display()
        .to_string();

    #[cfg(unix)]
    println!("{}", serde_json::json!({ "file": path_str, "mode": "0600" }));
    #[cfg(not(unix))]
    println!(
        "{}",
        serde_json::json!({ "file": path_str, "mode": "unrestricted" })
    );

    Ok(())
}

/// Best-effort: read the token cached by the official `gh` CLI. Any failure
/// path (gh not on PATH, not logged in, non-zero exit, empty stdout) falls
/// through to the next source. `gh auth token` is fast in practice; we don't
/// add an explicit timeout because a user can ctrl-c and `gh` itself doesn't
/// hang on cached credentials.
fn try_gh_cli_token() -> Option<String> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let trimmed = s.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(unix)]
fn write_token_file(path: &std::path::Path, token: &str) -> Result<()> {
    use std::fs::DirBuilder;
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    // Ensure parent directory exists with mode 0o700.
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|e| anyhow!("failed to create config dir: {e}"))?;
    }

    // Create or truncate with mode 0o600. The `mode` on `OpenOptions` only
    // applies at creation time; `set_permissions` below re-asserts 0o600 so
    // an existing file that was loosened gets tightened back on overwrite.
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| anyhow!("failed to open token file: {e}"))?;
    file.write_all(token.as_bytes())
        .map_err(|e| anyhow!("failed to write token: {e}"))?;
    drop(file);

    // Re-assert permissions in case the file pre-existed with looser bits.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(|e| anyhow!("failed to set permissions: {e}"))?;

    Ok(())
}

#[cfg(not(unix))]
fn write_token_file(path: &std::path::Path, token: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow!("failed to create config dir: {e}"))?;
        }
    }
    std::fs::write(path, token).map_err(|e| anyhow!("failed to write token file: {e}"))?;
    Ok(())
}

async fn workspace_dispatch(cmd: WorkspaceCmd, svc: &Services) -> Result<()> {
    match cmd {
        WorkspaceCmd::Create {
            name,
            description,
            local_only,
        } => {
            let dto = svc
                .workspaces
                .create(CreateWorkspaceCmd {
                    name,
                    description,
                    local_only,
                })
                .await?;
            render::workspace(&dto);
        }
        WorkspaceCmd::List { include_archived } => {
            let rows = svc
                .workspaces
                .list(ListWorkspacesQuery { include_archived })
                .await?;
            render::workspaces(&rows);
        }
        WorkspaceCmd::Show { id } => render::workspace(&svc.workspaces.show(&id).await?),
        WorkspaceCmd::Activate { id } => {
            render::workspace(&svc.workspaces.activate(&id).await?)
        }
        WorkspaceCmd::Pause { id } => render::workspace(&svc.workspaces.pause(&id).await?),
        WorkspaceCmd::Archive { id } => {
            render::workspace(&svc.workspaces.archive(&id).await?)
        }
    }
    Ok(())
}

async fn repo_dispatch(cmd: RepoCmd, svc: &Services) -> Result<()> {
    match cmd {
        RepoCmd::Attach {
            workspace,
            url,
            canonical,
            branch,
            path,
            no_link,
        } => {
            let link_path = resolve_attach_link_path(path.as_deref(), no_link, &canonical)?;

            let outcome = svc
                .bindings
                .attach(AttachRepoCmd {
                    workspace_id: workspace,
                    remote_url: url,
                    canonical_url: canonical,
                    tracked_branch: branch.clone(),
                    link_path,
                    link_branch: branch,
                })
                .await?;
            render::attach_outcome(&outcome);
        }
        RepoCmd::Detach { id } => {
            svc.bindings.detach(&id).await?;
            println!("{}", serde_json::json!({ "detached": id }));
        }
        RepoCmd::List { workspace } => {
            render::repos(&svc.bindings.list(&workspace).await?)
        }
        RepoCmd::Show { id } => render::repo(&svc.bindings.show(&id).await?),
        RepoCmd::Discover { path } => {
            let mut rows = Vec::new();
            for repo_path in discover_repos_under(&path) {
                let canonical = discover_canonical(&repo_path).ok().flatten();
                rows.push(DiscoveredRepo {
                    path: repo_path.display().to_string(),
                    canonical,
                });
            }
            render::discovered(&rows);
        }
        RepoCmd::Locate { path } => {
            let candidate = match path {
                Some(p) => p,
                None => std::env::current_dir()
                    .map_err(|e| anyhow!("failed to determine current directory: {e}"))?,
            };
            let abs = std::fs::canonicalize(&candidate)
                .unwrap_or_else(|_| candidate.clone());
            let query_path = abs.display().to_string();

            let canonical_url = match discover_canonical(&abs) {
                Err(_) | Ok(None) => None,
                Ok(Some(c)) => Some(c),
            };

            let matches = if let Some(ref canonical) = canonical_url {
                let workspaces = svc
                    .workspaces
                    .list(ListWorkspacesQuery::default())
                    .await?;
                let mut found: Vec<LocateMatchDto> = Vec::new();
                for ws in &workspaces {
                    let ws_id: domain_core::WorkspaceId = ws
                        .id
                        .parse()
                        .map_err(|e| anyhow!("invalid workspace id '{}': {e}", ws.id))?;
                    if let Some(binding) = svc
                        .bindings_repo
                        .find_by_canonical_url(ws_id, canonical)
                        .await
                        .map_err(|e| anyhow!("{e}"))?
                    {
                        found.push(LocateMatchDto {
                            workspace_id: ws.id.clone(),
                            binding: application_workspace::binding_to_dto(&binding),
                        });
                    }
                }
                found
            } else {
                vec![]
            };

            render::locate(&LocateResponseDto {
                query_path,
                canonical_url,
                matches,
            });
        }
    }
    Ok(())
}

/// Resolve the path that `repo attach` should register as a worktree.
///
/// Returns `Ok(None)` when the caller opted out via `--no-link`.
/// Otherwise discovers the cwd (or the explicit `--path`), verifies its
/// git origin canonicalises to `expected_canonical`, and returns the
/// absolute path string. All failure modes bail with a CLI-friendly
/// message that names the available escape hatches.
fn resolve_attach_link_path(
    path: Option<&std::path::Path>,
    no_link: bool,
    expected_canonical: &str,
) -> Result<Option<String>> {
    if no_link {
        return Ok(None);
    }

    let explicit_path = path.is_some();
    let candidate = match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|e| anyhow!("failed to determine current directory: {e}"))?,
    };
    let abs = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());

    match discover_canonical(&abs) {
        Err(infra_git::GitError::NotARepo(_)) if explicit_path => anyhow::bail!(
            "path is not a git repo: {}; pass a different --path or --no-link",
            abs.display()
        ),
        Err(infra_git::GitError::NotARepo(_)) => anyhow::bail!(
            "cwd is not a git repo: {}; pass --path <p> or --no-link",
            abs.display()
        ),
        Err(e) => return Err(anyhow!("{e}")),
        Ok(None) => anyhow::bail!(
            "git repo at {} has no `origin` remote; pass --path <p> or --no-link",
            abs.display()
        ),
        Ok(Some(discovered)) if discovered != expected_canonical => anyhow::bail!(
            "path origin canonicalises to '{discovered}', not '{expected_canonical}'; pass --path or --no-link"
        ),
        Ok(Some(_)) => Ok(Some(abs.display().to_string())),
    }
}

#[derive(serde::Serialize)]
pub struct DiscoveredRepo {
    pub path: String,
    pub canonical: Option<String>,
}

async fn worktree_dispatch(cmd: WorktreeCmd, svc: &Services) -> Result<()> {
    match cmd {
        WorktreeCmd::Link { repo, path, branch } => {
            let raw_path = std::path::Path::new(&path);
            let abs_path = std::fs::canonicalize(raw_path)
                .unwrap_or_else(|_| raw_path.to_path_buf());

            let discovered = match discover_canonical(&abs_path) {
                Err(infra_git::GitError::NotARepo(_)) => {
                    anyhow::bail!("path is not a git repo: {}", abs_path.display());
                }
                Err(e) => return Err(anyhow!("{e}")),
                Ok(None) => {
                    anyhow::bail!(
                        "git repo at {} has no `origin` remote",
                        abs_path.display()
                    );
                }
                Ok(Some(c)) => c,
            };

            let binding = svc.bindings.show(&repo).await?;
            if discovered != binding.canonical_url {
                // Try to find a matching binding across all workspaces.
                let workspaces = svc
                    .workspaces
                    .list(ListWorkspacesQuery::default())
                    .await?;
                let mut found_id: Option<String> = None;
                'outer: for ws in &workspaces {
                    let ws_id: domain_core::WorkspaceId = ws
                        .id
                        .parse()
                        .map_err(|e| anyhow!("invalid workspace id '{}': {e}", ws.id))?;
                    if let Some(b) = svc
                        .bindings_repo
                        .find_by_canonical_url(ws_id, &discovered)
                        .await
                        .map_err(|e| anyhow!("{e}"))?
                    {
                        found_id = Some(b.id.to_string());
                        break 'outer;
                    }
                }
                let repo_short = &repo;
                if let Some(found) = found_id {
                    anyhow::bail!(
                        "path origin '{discovered}' doesn't match repo {repo_short} \
                         ('{}'); use --repo {found} instead",
                        binding.canonical_url
                    );
                } else {
                    anyhow::bail!(
                        "path origin '{discovered}' doesn't match repo \
                         ('{}') and no binding matches '{discovered}'; \
                         run `rl repo attach` first",
                        binding.canonical_url
                    );
                }
            }

            let dto = svc
                .bindings
                .link_worktree(LinkWorktreeCmd {
                    repo_id: repo,
                    path: abs_path.display().to_string(),
                    branch,
                })
                .await?;
            render::repo(&dto);
        }
        WorktreeCmd::Unlink { repo, path } => {
            let dto = svc
                .bindings
                .unlink_worktree(UnlinkWorktreeCmd {
                    repo_id: repo,
                    path,
                })
                .await?;
            render::repo(&dto);
        }
        WorktreeCmd::PruneMissing { repo } => {
            let dto = svc.bindings.prune_missing(&repo).await?;
            render::repo(&dto);
        }
        WorktreeCmd::Reconcile { workspace, prune } => {
            let probe = TokioFilesystemProbe::new();
            let summary = svc
                .bindings
                .reconcile_worktrees(&workspace, &probe, prune)
                .await?;
            render::reconcile(&summary);
        }
    }
    Ok(())
}

/// Read `git config user.name` from the surrounding git repo. Returns
/// `None` if git isn't on PATH, the cwd isn't inside a repo, or the value
/// is empty. Used as a sensible default for `query mine --assignee`.
fn git_user_name() -> Option<String> {
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

async fn task_dispatch(cmd: TaskCmd, svc: &Services) -> Result<()> {
    match cmd {
        TaskCmd::Create {
            workspace,
            repo,
            title,
            body,
            priority,
        } => {
            let dto = svc
                .tasks
                .create(CreateTaskCmd {
                    workspace_id: workspace,
                    repo_id: repo,
                    title,
                    body,
                    priority,
                })
                .await?;
            render::task(&dto);
        }
        TaskCmd::Show { id } => render::task(&svc.tasks.show(&id).await?),
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
            batch_task_op(tasks, |id| async move { svc.tasks.stage_for_sync(&id).await }).await?;
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
        TaskCmd::Relate { id, kind, other } => {
            let dto = svc
                .tasks
                .add_relation(AddTaskRelationCmd {
                    task_id: id,
                    kind,
                    other,
                })
                .await?;
            render::task(&dto);
        }
        TaskCmd::Snapshots { id } => {
            let task_id: domain_core::TaskId =
                id.parse().map_err(|e| anyhow!("invalid task id: {e}"))?;
            let snaps = svc.tasks.snapshots_repo().list(task_id).await
                .map_err(|e| anyhow!("{e}"))?;
            render::snapshots(&snaps);
        }
        TaskCmd::Rollback { id, to_version } => {
            let dto = svc.tasks.rollback(&id, to_version).await
                .map_err(|e| anyhow!("{e}"))?;
            render::task(&dto);
        }
    }
    Ok(())
}

async fn query_dispatch(cmd: QueryCmd, svc: &Services, cfg: &RepoLinkConfig) -> Result<()> {
    match cmd {
        QueryCmd::Overview { workspace } => {
            let v = svc.query.overview(&workspace).await?;
            render::overview(&v);
        }
        QueryCmd::Blocked { workspace } => {
            let v = svc.query.blocked_tasks(&workspace).await?;
            render::blocked(&v);
        }
        QueryCmd::Stale { workspace } => {
            let v = svc.query.stale_worktrees(&workspace).await?;
            render::stale(&v);
        }
        QueryCmd::Unsynced { workspace } => {
            let v = svc.query.unsynced_tasks(&workspace).await?;
            render::unsynced(&v);
        }
        QueryCmd::Contributors { workspace } => {
            let v = svc.query.contributors(&workspace).await?;
            render::contributors(&v);
        }
        QueryCmd::Drift { workspace } => {
            let v = svc.query.drift(&workspace).await?;
            render::drift(&v);
        }
        QueryCmd::Ready { workspace } => {
            let v = svc.query.ready_tasks(&workspace).await?;
            render::ready(&v);
        }
        QueryCmd::Mine { workspace, assignee } => {
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
