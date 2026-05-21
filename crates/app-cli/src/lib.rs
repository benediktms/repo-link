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
    ListTasksQuery, ListWorkspacesQuery, UnlinkWorktreeCmd,
};
use infra_config::RepoLinkConfig;
use infra_filesystem::{TokioFilesystemProbe, discover_repos_under};
use infra_git::discover_canonical;
use infra_github::GithubTaskProvider;
use infra_sqlite::{
    SqliteRepoBindingRepository, SqliteTaskRepository, SqliteWorkspaceRepository, open_from_path,
};

mod render;

#[derive(Parser, Debug)]
#[command(name = "repo-link", version, about = "Local-first workspace + task manager")]
struct Cli {
    /// SQLite database path. Falls back to platform data dir.
    #[arg(long, env = "REPO_LINK_DB", global = true)]
    db: Option<PathBuf>,

    /// Emit JSON instead of a table.
    #[arg(long, global = true)]
    json: bool,

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
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        include_archived: bool,
    },
    Stage {
        id: String,
    },
    Block {
        id: String,
    },
    Archive {
        id: String,
    },
    Relate {
        id: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        other: String,
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

struct Services {
    workspaces: WorkspaceService,
    bindings: RepoBindingService,
    tasks: TaskService,
    query: QueryService,
    sync: Option<SyncService>,
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
        Arc::new(SqliteTaskRepository::new(db));

    // Sync is only available when a GitHub token resolved from config.
    let sync = cfg.github_token.clone().map(|token| {
        let provider: Arc<dyn ports::RemoteTaskProvider> =
            Arc::new(GithubTaskProvider::new(token));
        SyncService::new(tasks_repo.clone(), bindings_repo.clone(), provider)
    });

    Ok(Services {
        workspaces: WorkspaceService::new(workspaces_repo.clone()),
        bindings: RepoBindingService::new(workspaces_repo.clone(), bindings_repo.clone()),
        tasks: TaskService::new(tasks_repo.clone()),
        query: QueryService::new(workspaces_repo, bindings_repo, tasks_repo),
        sync,
    })
}

async fn dispatch(cli: Cli, svc: &Services, cfg: &RepoLinkConfig) -> Result<()> {
    let json = cli.json;
    match cli.cmd {
        Cmd::Workspace(c) => workspace_dispatch(c, svc, json).await,
        Cmd::Repo(c) => repo_dispatch(c, svc, json).await,
        Cmd::Worktree(c) => worktree_dispatch(c, svc, json).await,
        Cmd::Task(c) => task_dispatch(c, svc, json).await,
        Cmd::Query(c) => query_dispatch(c, svc, cfg, json).await,
        Cmd::Sync(c) => sync_dispatch(c, svc, json).await,
    }
}

async fn sync_dispatch(cmd: SyncCmd, svc: &Services, json: bool) -> Result<()> {
    let sync = svc.sync.as_ref().ok_or_else(|| {
        anyhow!("sync requires REPO_LINK_GITHUB_TOKEN or GITHUB_TOKEN to be set")
    })?;
    let summary = match cmd {
        SyncCmd::Promote { task } => sync.promote(&task).await?,
        SyncCmd::Push { task } => sync.push(&task).await?,
        SyncCmd::Pull { task } => sync.pull(&task).await?,
    };
    render::sync(&summary, json);
    Ok(())
}

async fn workspace_dispatch(cmd: WorkspaceCmd, svc: &Services, json: bool) -> Result<()> {
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
            render::workspace(&dto, json);
        }
        WorkspaceCmd::List { include_archived } => {
            let rows = svc
                .workspaces
                .list(ListWorkspacesQuery { include_archived })
                .await?;
            render::workspaces(&rows, json);
        }
        WorkspaceCmd::Show { id } => render::workspace(&svc.workspaces.show(&id).await?, json),
        WorkspaceCmd::Activate { id } => {
            render::workspace(&svc.workspaces.activate(&id).await?, json)
        }
        WorkspaceCmd::Pause { id } => render::workspace(&svc.workspaces.pause(&id).await?, json),
        WorkspaceCmd::Archive { id } => {
            render::workspace(&svc.workspaces.archive(&id).await?, json)
        }
    }
    Ok(())
}

async fn repo_dispatch(cmd: RepoCmd, svc: &Services, json: bool) -> Result<()> {
    match cmd {
        RepoCmd::Attach {
            workspace,
            url,
            canonical,
            branch,
        } => {
            let dto = svc
                .bindings
                .attach(AttachRepoCmd {
                    workspace_id: workspace,
                    remote_url: url,
                    canonical_url: canonical,
                    tracked_branch: branch,
                })
                .await?;
            render::repo(&dto, json);
        }
        RepoCmd::Detach { id } => {
            svc.bindings.detach(&id).await?;
            if json {
                println!("{}", serde_json::json!({ "detached": id }));
            } else {
                println!("detached {id}");
            }
        }
        RepoCmd::List { workspace } => {
            render::repos(&svc.bindings.list(&workspace).await?, json)
        }
        RepoCmd::Show { id } => render::repo(&svc.bindings.show(&id).await?, json),
        RepoCmd::Discover { path } => {
            let mut rows = Vec::new();
            for repo_path in discover_repos_under(&path) {
                let canonical = discover_canonical(&repo_path).ok().flatten();
                rows.push(DiscoveredRepo {
                    path: repo_path.display().to_string(),
                    canonical,
                });
            }
            render::discovered(&rows, json);
        }
    }
    Ok(())
}

#[derive(serde::Serialize)]
pub struct DiscoveredRepo {
    pub path: String,
    pub canonical: Option<String>,
}

async fn worktree_dispatch(cmd: WorktreeCmd, svc: &Services, json: bool) -> Result<()> {
    match cmd {
        WorktreeCmd::Link { repo, path, branch } => {
            let dto = svc
                .bindings
                .link_worktree(LinkWorktreeCmd {
                    repo_id: repo,
                    path,
                    branch,
                })
                .await?;
            render::repo(&dto, json);
        }
        WorktreeCmd::Unlink { repo, path } => {
            let dto = svc
                .bindings
                .unlink_worktree(UnlinkWorktreeCmd {
                    repo_id: repo,
                    path,
                })
                .await?;
            render::repo(&dto, json);
        }
        WorktreeCmd::PruneMissing { repo } => {
            let dto = svc.bindings.prune_missing(&repo).await?;
            render::repo(&dto, json);
        }
        WorktreeCmd::Reconcile { workspace, prune } => {
            let probe = TokioFilesystemProbe::new();
            let summary = svc
                .bindings
                .reconcile_worktrees(&workspace, &probe, prune)
                .await?;
            render::reconcile(&summary, json);
        }
    }
    Ok(())
}

async fn task_dispatch(cmd: TaskCmd, svc: &Services, json: bool) -> Result<()> {
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
            render::task(&dto, json);
        }
        TaskCmd::Show { id } => render::task(&svc.tasks.show(&id).await?, json),
        TaskCmd::List {
            workspace,
            state,
            include_archived,
        } => {
            let rows = svc
                .tasks
                .list(ListTasksQuery {
                    workspace_id: workspace,
                    repo_id: None,
                    state,
                    include_archived,
                })
                .await?;
            render::tasks(&rows, json);
        }
        TaskCmd::Stage { id } => render::task(&svc.tasks.stage_for_sync(&id).await?, json),
        TaskCmd::Block { id } => render::task(&svc.tasks.mark_blocked(&id).await?, json),
        TaskCmd::Archive { id } => render::task(&svc.tasks.archive(&id).await?, json),
        TaskCmd::Relate { id, kind, other } => {
            let dto = svc
                .tasks
                .add_relation(AddTaskRelationCmd {
                    task_id: id,
                    kind,
                    other,
                })
                .await?;
            render::task(&dto, json);
        }
    }
    Ok(())
}

async fn query_dispatch(cmd: QueryCmd, svc: &Services, cfg: &RepoLinkConfig, json: bool) -> Result<()> {
    match cmd {
        QueryCmd::Overview { workspace } => {
            let v = svc.query.overview(&workspace).await?;
            render::overview(&v, json);
        }
        QueryCmd::Blocked { workspace } => {
            let v = svc.query.blocked_tasks(&workspace).await?;
            render::blocked(&v, json);
        }
        QueryCmd::Stale { workspace } => {
            let v = svc.query.stale_worktrees(&workspace).await?;
            render::stale(&v, json);
        }
        QueryCmd::Unsynced { workspace } => {
            let v = svc.query.unsynced_tasks(&workspace).await?;
            render::unsynced(&v, json);
        }
        QueryCmd::Contributors { workspace } => {
            let v = svc.query.contributors(&workspace).await?;
            render::contributors(&v, json);
        }
        QueryCmd::Drift { workspace } => {
            let v = svc.query.drift(&workspace).await?;
            render::drift(&v, json);
        }
        QueryCmd::Ready { workspace } => {
            let v = svc.query.ready_tasks(&workspace).await?;
            render::ready(&v, json);
        }
        QueryCmd::Mine { workspace, assignee } => {
            let assignee = assignee
                .or_else(|| cfg.default_user.clone())
                .ok_or_else(|| anyhow!("set --assignee, REPO_LINK_USER, or USER"))?;
            let v = svc.query.assigned_to(&workspace, &assignee).await?;
            render::assigned(&v, json);
        }
    }
    Ok(())
}
