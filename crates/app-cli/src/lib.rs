//! repo-link CLI — also installed as `rl`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use application_project::ProjectService;
use application_query::QueryService;
use application_sync::SyncService;
use application_task::TaskService;
use application_workspace::{RepoBindingService, WorkspaceService};
use clap::{Args, Parser, Subcommand};
use dto_shared::{
    AddTaskRelationCmd, AttachRepoCmd, CreateTaskCmd, CreateWorkspaceCmd, ImportMirrorCmd,
    LinkProjectCmd, LinkWorktreeCmd, ListTasksQuery, ListWorkspacesQuery, LocateResponseDto,
    MapStatusCmd, StatusMappingDto, StatusOptionDto, UnlinkWorktreeCmd, UpdateTaskCmd,
};
use infra_config::RepoLinkConfig;
use infra_filesystem::{TokioFilesystemProbe, discover_repos_under};
use infra_git::discover_canonical;
use infra_github::GithubTaskProvider;
use infra_sqlite::{
    SqliteProjectRepository, SqliteRepoBindingRepository, SqliteTaskRepository,
    SqliteTaskSnapshotRepository, SqliteWorkspaceRepository, open_from_path,
};

mod daemon;
mod docs;
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

// Shared `#[command(flatten)]` arg groups. One definition per concept,
// reused by every variant that needs it — short/long mapping, help text,
// and any future env var or alias live in exactly one place.

#[derive(Args, Debug)]
struct WorkspaceArg {
    /// Workspace UUID.
    #[arg(short = 'w', long)]
    workspace: String,
}

#[derive(Args, Debug)]
struct TaskArg {
    /// Task reference: UUID, bare hash, or `prefix-hash`.
    #[arg(short = 't', long)]
    task: String,
}

#[derive(Args, Debug)]
struct BranchArg {
    /// Tracked branch.
    #[arg(short = 'b', long)]
    branch: Option<String>,
}

#[derive(Args, Debug)]
struct AliasArg {
    /// Alias string.
    #[arg(short = 'a', long)]
    alias: String,
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
    /// Documentation helpers for AI agents picking up this repo.
    #[command(subcommand)]
    Agents(AgentsCmd),
    /// GitHub Projects v2 management (local-only in Stage 4 — `rl project link`
    /// accepts hand-entered schema; Stage 5 swaps the GraphQL fetch in).
    #[command(subcommand)]
    Project(ProjectCmd),
    /// Manage the background reconciliation daemon (launchd / systemd unit).
    #[command(subcommand)]
    Daemon(daemon::DaemonCmd),
}

#[derive(Subcommand, Debug)]
enum WorkspaceCmd {
    Create {
        name: String,
        #[arg(short = 'd', long)]
        description: Option<String>,
        #[arg(long)]
        local_only: bool,
        /// Optional GitHub Projects v2 board to attach the new workspace
        /// to. Accepts a project node ID (`PVT_…`) or `owner/number`.
        /// The project must already be linked locally — see `rl project link`.
        #[arg(long)]
        project: Option<String>,
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
    /// Attach a workspace to a project (or detach with `--none`). Resolves
    /// `<project>` as a node ID or `owner/number`, same as
    /// `rl project show`.
    SetProject {
        workspace: String,
        /// Project to attach the workspace to (`PVT_…` or `owner/number`).
        /// Mutually exclusive with `--none`.
        #[arg(long, conflicts_with = "none")]
        project: Option<String>,
        /// Detach the workspace from any project. Mutually exclusive with
        /// `--project`.
        #[arg(long)]
        none: bool,
    },
}

#[derive(Subcommand, Debug)]
enum RepoCmd {
    Attach {
        #[command(flatten)]
        ws: WorkspaceArg,
        #[arg(short = 'u', long)]
        url: String,
        #[arg(short = 'c', long)]
        canonical: String,
        #[command(flatten)]
        br: BranchArg,
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
        #[arg(short = 'p', long)]
        path: Option<PathBuf>,
        /// Skip auto-linking the current path. Use this when you're
        /// sitting in one clone but don't want it recorded under this
        /// binding — e.g. you have the same repo cloned twice and
        /// only the *other* clone should be the tracked checkout.
        /// Combine with `--path <other-clone>` (or follow up with
        /// `rl worktree link`) to register the intended path instead.
        #[arg(long)]
        no_link: bool,
        /// Override the auto-derived short prefix for this binding
        /// (e.g. `--prefix gw` instead of letting the algorithm pick
        /// `pck` from `app-packages`). Must match
        /// `^[a-z][a-z0-9]{1,19}$`. Conflicts with another binding's
        /// prefix surface as a hard error — pick a different value.
        /// Omit to let the system derive and collision-break itself.
        #[arg(long)]
        prefix: Option<String>,
    },
    /// Detach a binding. Accepts the same handle forms as `rl repo show`:
    /// UUID / prefix / name / alias. Ambiguous matches exit 2 with a
    /// candidate list.
    Detach { id: String },
    List {
        #[command(flatten)]
        ws: WorkspaceArg,
    },
    /// Show a binding. Accepts a UUID, an exact `name`, or an exact alias.
    /// Returns a JSON error with candidate IDs if a non-UUID handle matches
    /// more than one binding — re-issue with a UUID.
    Show { id: String },
    /// Walk a directory and report every git repo found, with its origin URL.
    /// Use this to populate a workspace from `~/code/` in one shot.
    Discover {
        #[arg(short = 'p', long)]
        path: PathBuf,
    },
    /// Discover which repo binding (if any) owns the given path.
    /// Reads the path's git origin, canonicalises it, and looks for a
    /// matching binding across all non-archived workspaces.
    Locate {
        /// Path to probe. Defaults to current working directory.
        #[arg(short = 'p', long)]
        path: Option<PathBuf>,
    },
    /// Set a new short name on a binding. Identity stays at canonical_url —
    /// rename is purely a display affordance.
    Rename {
        #[arg(long)]
        repo: String,
        #[arg(short = 'n', long)]
        name: String,
    },
    /// Replace the binding's globally-unique short prefix (e.g. swap an
    /// auto-derived `pck` for a manual `gw`). Must match
    /// `^[a-z][a-z0-9]{1,19}$`. Conflicts with another binding's prefix
    /// surface as a hard error — pick a different value.
    ///
    /// Warning: every composite task ID a user has already typed
    /// against the *old* prefix (e.g. `oldpfx-ak7`) goes stale and
    /// errors with `PrefixMismatch`. Bare-hash references (`ak7`) keep
    /// working because the hash itself is globally unique.
    SetPrefix {
        #[arg(long)]
        repo: String,
        /// New prefix value. Must match `^[a-z][a-z0-9]{1,19}$`.
        #[arg(short = 'p', long)]
        prefix: String,
    },
    /// Manage aliases — alternative short names for a binding.
    #[command(subcommand)]
    Alias(RepoAliasCmd),
    /// Search bindings across non-archived workspaces by name / alias /
    /// canonical substring. Ranked: exact name > exact alias > canonical
    /// substring > name substring. `ambiguous` is set when more than one
    /// hit is returned.
    Find { query: String },
}

#[derive(Subcommand, Debug)]
enum RepoAliasCmd {
    Add {
        #[arg(long)]
        repo: String,
        #[command(flatten)]
        a: AliasArg,
    },
    Rm {
        #[arg(long)]
        repo: String,
        #[command(flatten)]
        a: AliasArg,
    },
}

#[derive(Subcommand, Debug)]
enum WorktreeCmd {
    Link {
        /// Repo binding, by UUID / prefix / name / alias (same forms as
        /// `rl repo show`).
        #[arg(long)]
        repo: String,
        #[arg(short = 'p', long)]
        path: String,
        #[command(flatten)]
        br: BranchArg,
    },
    Unlink {
        /// Repo binding, by UUID / prefix / name / alias (same forms as
        /// `rl repo show`).
        #[arg(long)]
        repo: String,
        #[arg(short = 'p', long)]
        path: String,
    },
    PruneMissing {
        /// Repo binding, by UUID / prefix / name / alias (same forms as
        /// `rl repo show`).
        #[arg(long)]
        repo: String,
    },
    /// Scan every worktree in a workspace, mark missing paths, optionally
    /// drop them. Use this after switching machines or pruning checkouts.
    Reconcile {
        #[command(flatten)]
        ws: WorkspaceArg,
        #[arg(long)]
        prune: bool,
    },
}

#[derive(Subcommand, Debug)]
enum TaskCmd {
    Create {
        #[command(flatten)]
        ws: WorkspaceArg,
        /// Owning repo binding, by UUID / prefix / name / alias (same forms
        /// as `rl repo show`).
        #[arg(short = 'r', long)]
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
    /// Edit a task in place. Writes a new snapshot at `version = max + 1`
    /// with `source = local_edit`; preserves the task's identity (UUID and
    /// short prefix). At least one of `--title`, `--body`, `--priority`,
    /// `--assignee`, or `--repo` must be supplied.
    Edit {
        id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        priority: Option<String>,
        /// Replace-set: each `--assignee` flag adds one entry; the full
        /// list replaces the current assignees. Omitting `--assignee`
        /// entirely leaves the existing assignees untouched. There is no
        /// way to clear assignees via `edit` — that's a deliberate gap
        /// (matches the spec).
        #[arg(long = "assignee")]
        assignees: Vec<String>,
        /// Reassign the task's owning repo binding, by UUID / prefix / name /
        /// alias (same forms as `rl repo show`). Use this to attach a repo to
        /// a task created without one — required before `sync promote`, which
        /// needs a repo to know which GitHub repo to open the issue in. Only
        /// valid while the task is not yet synced to a remote issue;
        /// reassigning a synced task is rejected.
        #[arg(short = 'r', long)]
        repo: Option<String>,
    },
    List {
        #[arg(short = 'w', long)]
        workspace: Option<String>,
        /// Filter by lifecycle status (`open` / `in_progress` / `blocked` / `done` / `archived`).
        #[arg(short = 's', long)]
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
    /// Local-only lifecycle nudge: Open|Blocked → InProgress.
    ///
    /// Flips the task to `InProgress` so your local queries (`query ready`,
    /// `query mine`) reflect reality. Does NOT touch `assignees` and does NOT
    /// push to GitHub — teammates won't see anything change. Works on purely-
    /// local tasks. Offline-safe. Use `task claim` instead when you want to
    /// announce externally that you've picked up the task.
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
    /// Publicly take ownership of a task: assign + start + push in one shot.
    ///
    /// Use this — instead of `task start` — the moment you want teammates,
    /// the GitHub issue list, and project boards to know you've picked
    /// the task up. The lifecycle move is the same as `start`; the
    /// difference is that `claim` ALSO updates `assignees` and mirrors
    /// the change to GitHub.
    ///
    /// Pipeline (per task):
    /// 1. Add the authenticated GitHub user to `assignees` (merge — leaves
    ///    teammates intact; no-op if you're already an assignee).
    /// 2. Transition `Open`|`Blocked` → `InProgress` (no-op if already
    ///    in-progress).
    /// 3. Best-effort `sync push` to mirror the new state to the remote
    ///    issue. Local-only / staged tasks skip the push with a hint to
    ///    promote first.
    ///
    /// Refuses on `Done` / `Archived`. Requires the cached GitHub login
    /// (`rl gh auth` populates it); without one, errors with a re-auth
    /// hint before touching any task state.
    Claim {
        #[arg(required = true)]
        tasks: Vec<String>,
        /// Apply locally only; skip the GitHub push step.
        #[arg(long)]
        no_sync: bool,
    },
    /// Add a pending local comment to a task. Pushed to the remote issue on
    /// the next `sync push` (a separate axis — does not dirty the task).
    Comment {
        id: String,
        body: String,
    },
    /// Re-wire a task to a different remote issue. Always flips the task to
    /// `Conflict` (linking is destructive on remote identity; snapshots are
    /// the audit trail). Pass `--relink/-r` to declare the URL is the verified
    /// redirect target of the current remote (after a GitHub transfer) — in
    /// that case identity is preserved and the task stays in its existing
    /// sync state. Target repo must already be attached via `rl repo attach`.
    Link {
        id: String,
        url: String,
        #[arg(long, short = 'r')]
        relink: bool,
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
        #[command(flatten)]
        ws: WorkspaceArg,
    },
    Blocked {
        #[command(flatten)]
        ws: WorkspaceArg,
    },
    Stale {
        #[command(flatten)]
        ws: WorkspaceArg,
    },
    Unsynced {
        #[command(flatten)]
        ws: WorkspaceArg,
    },
    Contributors {
        #[command(flatten)]
        ws: WorkspaceArg,
    },
    Drift {
        #[command(flatten)]
        ws: WorkspaceArg,
    },
    /// Tasks that are actionable now: open + not transitively blocked.
    Ready {
        #[command(flatten)]
        ws: WorkspaceArg,
    },
    /// Open tasks assigned to a user. Defaults to $REPO_LINK_USER or $USER.
    Mine {
        #[command(flatten)]
        ws: WorkspaceArg,
        #[arg(long, env = "REPO_LINK_USER")]
        assignee: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum SyncCmd {
    /// Create the remote issue for a Draft/Staged task.
    Promote {
        #[command(flatten)]
        t: TaskArg,
    },
    /// Push local edits (state = DirtyLocal) to the remote.
    Push {
        #[command(flatten)]
        t: TaskArg,
    },
    /// Pull the latest remote snapshot and reconcile.
    Pull {
        #[command(flatten)]
        t: TaskArg,
    },
    /// Import a GitHub issue by URL as a local task, optionally cascading
    /// into its sub-issues.
    Import {
        /// GitHub issue URL, e.g. https://github.com/owner/repo/issues/123.
        url: String,
        /// Also import the issue's sub-issue tree (recursively), wiring
        /// `child_of` relations. Cross-repo sub-issues are skipped.
        #[arg(long)]
        cascade: bool,
        #[command(flatten)]
        ws: WorkspaceArg,
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

#[derive(Subcommand, Debug)]
enum AgentsCmd {
    /// Render a self-documenting `rl` block into `./AGENTS.md`.
    ///
    /// Splices between `<!-- rl:doc:start -->` and `<!-- rl:doc:end -->`,
    /// creating the file if missing or appending the block if no markers
    /// are present. Always rewrites the block on every run.
    Docs,
}

#[derive(Subcommand, Debug)]
enum ProjectCmd {
    /// Link a project locally with hand-entered schema. Stage 5 will
    /// rewire this to fetch the schema from GitHub; the local model and
    /// CLI shape stay the same either way.
    ///
    /// `--option` takes `<option-id>:<name>` and is repeatable; the
    /// option's `ordinal` is the order it appears on the command line.
    /// `--map` takes `<status>:<option-id>` and seeds initial mappings.
    /// Many-to-one mappings (multiple statuses → one option) are valid
    /// in the domain but currently lossy on save — see #80.
    Link {
        #[arg(long)]
        node_id: String,
        #[arg(long)]
        owner: String,
        #[arg(long)]
        number: u64,
        #[arg(long)]
        title: String,
        #[arg(long)]
        status_field_id: String,
        /// Status field option as `<option-id>:<name>`. Repeat per option.
        #[arg(long = "option", value_parser = parse_option_kv)]
        options: Vec<(String, String)>,
        /// Initial mapping as `<status>:<option-id>`. Repeat per mapping.
        /// `<status>` is one of `open`, `in_progress`, `blocked`, `done`.
        #[arg(long = "map", value_parser = parse_mapping_kv)]
        mappings: Vec<(String, String)>,
    },
    /// List every locally-known project (across all workspaces).
    List,
    /// Show one project. `<spec>` is `owner/number` or a `PVT_…` node id.
    Show { spec: String },
    /// Set a local TaskStatus → project option mapping.
    Map {
        spec: String,
        /// Local task status (`open` / `in_progress` / `blocked` / `done`).
        #[arg(long)]
        local: String,
        /// Option ID on the project's Status field.
        #[arg(long = "option-id")]
        option_id: String,
    },
    /// Unlink a project locally. Workspaces attached to it have their
    /// `project_id` reset to NULL via the storage cascade.
    Unlink { spec: String },
}

/// Parse `<option-id>:<name>` into a tuple for clap's `value_parser`.
fn parse_option_kv(raw: &str) -> std::result::Result<(String, String), String> {
    let (id, name) = raw
        .split_once(':')
        .ok_or_else(|| format!("expected `<option-id>:<name>`, got {raw:?}"))?;
    if id.is_empty() || name.is_empty() {
        return Err(format!(
            "option-id and name must both be non-empty, got {raw:?}"
        ));
    }
    Ok((id.to_string(), name.to_string()))
}

/// Parse `<status>:<option-id>` into a tuple for clap's `value_parser`.
fn parse_mapping_kv(raw: &str) -> std::result::Result<(String, String), String> {
    let (status, opt) = raw
        .split_once(':')
        .ok_or_else(|| format!("expected `<status>:<option-id>`, got {raw:?}"))?;
    if status.is_empty() || opt.is_empty() {
        return Err(format!(
            "status and option-id must both be non-empty, got {raw:?}"
        ));
    }
    Ok((status.to_string(), opt.to_string()))
}

struct Services {
    workspaces: WorkspaceService,
    bindings: RepoBindingService,
    tasks: TaskService,
    query: QueryService,
    projects: ProjectService,
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
        Arc::new(SqliteTaskSnapshotRepository::new(db.clone()));
    let projects_repo: Arc<dyn ports::ProjectRepository> =
        Arc::new(SqliteProjectRepository::new(db));

    Ok(Services {
        workspaces: WorkspaceService::with_projects(workspaces_repo.clone(), projects_repo.clone()),
        bindings: RepoBindingService::new(workspaces_repo.clone(), bindings_repo.clone()),
        tasks: TaskService::new(tasks_repo.clone(), snapshots_repo, bindings_repo.clone()),
        query: QueryService::new(workspaces_repo, bindings_repo.clone(), tasks_repo.clone()),
        projects: ProjectService::new(projects_repo),
        tasks_repo,
        bindings_repo,
    })
}

async fn dispatch(cli: Cli, svc: &Services, cfg: &RepoLinkConfig) -> Result<()> {
    match cli.cmd {
        Cmd::Workspace(c) => workspace_dispatch(c, svc).await,
        Cmd::Repo(c) => repo_dispatch(c, svc).await,
        Cmd::Worktree(c) => worktree_dispatch(c, svc).await,
        Cmd::Task(c) => task_dispatch(c, svc, cfg).await,
        Cmd::Query(c) => query_dispatch(c, svc, cfg).await,
        Cmd::Sync(c) => sync_dispatch(c, svc, cfg).await,
        Cmd::Gh(c) => gh_dispatch(c, cfg).await,
        Cmd::Agents(c) => agents_dispatch(c, svc).await,
        Cmd::Project(c) => project_dispatch(c, svc).await,
        Cmd::Daemon(c) => daemon::dispatch(c, cfg).await,
    }
}

async fn agents_dispatch(cmd: AgentsCmd, svc: &Services) -> Result<()> {
    match cmd {
        AgentsCmd::Docs => {
            let cwd = std::env::current_dir()
                .map_err(|e| anyhow!("failed to read current directory: {e}"))?;
            let abs = std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());

            // Only "not a git repo" / "no origin" maps to None — other
            // errors (missing git, permission denied, etc.) surface as
            // hard failures so the agent sees the real cause.
            let canonical_url = match discover_canonical(&abs) {
                Err(infra_git::GitError::NotARepo(_)) | Ok(None) => None,
                Err(e) => return Err(anyhow!("{e}")),
                Ok(Some(c)) => Some(c),
            };

            let memberships = match canonical_url.as_deref() {
                Some(c) => svc
                    .bindings
                    .memberships_for_canonical_url(c)
                    .await?
                    .into_iter()
                    .map(|m| docs::DocRepoMembership {
                        workspace_id: m.workspace.id,
                        workspace_name: m.workspace.name,
                        binding_name: m.binding.name,
                        aliases: m.binding.aliases,
                        prefix: m.binding.prefix,
                    })
                    .collect(),
                None => Vec::new(),
            };

            let repo_info = docs::render_repo_info(&memberships, canonical_url.as_deref());
            let body = docs::render_block(&repo_info);
            let path = abs.join("AGENTS.md");
            let outcome = docs::write_agents_md(&path, &body)?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(())
        }
    }
}

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

async fn project_dispatch(cmd: ProjectCmd, svc: &Services) -> Result<()> {
    match cmd {
        ProjectCmd::Link {
            node_id,
            owner,
            number,
            title,
            status_field_id,
            options,
            mappings,
        } => {
            let status_options: Vec<StatusOptionDto> = options
                .into_iter()
                .enumerate()
                .map(|(i, (id, name))| StatusOptionDto {
                    option_id: id,
                    name,
                    // The CLI surface uses positional order as the user's
                    // intended display order; same as we'd get from the
                    // GraphQL field response in Stage 5.
                    ordinal: u32::try_from(i).unwrap_or(u32::MAX),
                    default_for: None,
                })
                .collect();
            let initial_mappings: Vec<StatusMappingDto> = mappings
                .into_iter()
                .map(|(status, option_id)| StatusMappingDto { status, option_id })
                .collect();
            let dto = svc
                .projects
                .link(LinkProjectCmd {
                    node_id,
                    owner_login: owner,
                    number,
                    title,
                    status_field_id,
                    status_options,
                    initial_mappings,
                })
                .await
                .map_err(|e| anyhow!("{e}"))?;
            println!("{}", serde_json::to_string_pretty(&dto)?);
        }
        ProjectCmd::List => {
            let dtos = svc.projects.list().await.map_err(|e| anyhow!("{e}"))?;
            println!("{}", serde_json::to_string_pretty(&dtos)?);
        }
        ProjectCmd::Show { spec } => {
            let dto = svc.projects.get(&spec).await.map_err(|e| anyhow!("{e}"))?;
            println!("{}", serde_json::to_string_pretty(&dto)?);
        }
        ProjectCmd::Map {
            spec,
            local,
            option_id,
        } => {
            let dto = svc
                .projects
                .map_status(MapStatusCmd {
                    project_spec: spec,
                    status: local,
                    option_id,
                })
                .await
                .map_err(|e| anyhow!("{e}"))?;
            println!("{}", serde_json::to_string_pretty(&dto)?);
        }
        ProjectCmd::Unlink { spec } => {
            svc.projects
                .unlink(&spec)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            println!("{}", serde_json::json!({ "unlinked": spec }));
        }
    }
    Ok(())
}

async fn sync_dispatch(cmd: SyncCmd, svc: &Services, cfg: &RepoLinkConfig) -> Result<()> {
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
        SyncCmd::Import { .. } => unreachable!("handled above"),
    };
    render::sync(&summary);
    Ok(())
}

/// Parse a GitHub issue URL into `(canonical "github.com/owner/repo", number)`.
/// Returns `None` for anything that isn't a `github.com/.../issues/<n>` URL.
fn parse_issue_url(url: &str) -> Option<(String, String)> {
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

async fn gh_dispatch(cmd: GhCmd, cfg: &RepoLinkConfig) -> Result<()> {
    match cmd {
        GhCmd::Auth { token, force } => gh_auth(token, force, cfg).await,
    }
}

async fn gh_auth(token: Option<String>, force: bool, cfg: &RepoLinkConfig) -> Result<()> {
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

    // Best-effort: fetch the authenticated user's login and cache it next to
    // the token. A network failure / invalid token shouldn't block the auth
    // flow — the token still gets persisted and downstream verbs that need
    // the login (e.g. `task claim`) report a clear "re-run rl gh auth" hint.
    let login = match build_github_provider(&trimmed, cfg) {
        Ok(provider) => match provider.current_user_login().await {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!(
                    "note: token saved, but couldn't fetch GitHub login ({e}). \
                     Re-run `rl gh auth` once connectivity / the token is good \
                     so commands like `rl task claim` can resolve your handle."
                );
                None
            }
        },
        Err(e) => {
            eprintln!("note: token saved, but provider init failed ({e}).");
            None
        }
    };

    write_token_file(&cfg.token_file_path, &trimmed, login.as_deref())?;

    let path_str = cfg
        .token_file_path
        .canonicalize()
        .unwrap_or_else(|_| cfg.token_file_path.clone())
        .display()
        .to_string();

    #[cfg(unix)]
    let mode_value = "0600";
    #[cfg(not(unix))]
    let mode_value = "unrestricted";

    let mut payload = serde_json::json!({ "file": path_str, "mode": mode_value });
    if let Some(l) = login.as_deref() {
        payload["login"] = serde_json::Value::String(l.to_string());
    }
    println!("{payload}");

    Ok(())
}

/// Construct a `GithubTaskProvider`, honoring `REPO_LINK_GITHUB_API_BASE_URL`
/// when set (for GitHub Enterprise or integration tests pointing at a
/// wiremock). Falls back to api.github.com.
fn build_github_provider(
    token: &str,
    cfg: &RepoLinkConfig,
) -> Result<GithubTaskProvider, ports::PortError> {
    match cfg.github_api_base_url.as_deref() {
        Some(url) => GithubTaskProvider::with_base_url(token, url),
        None => GithubTaskProvider::new(token),
    }
}

/// Resolve the GitHub token or fail with a command-specific "set token or
/// run `rl gh auth`" message. Centralised so the wording — including the
/// resolved token-file path — stays in one place.
fn require_github_token(cfg: &RepoLinkConfig, verb: &str) -> Result<String> {
    cfg.resolve_github_token()
        .map_err(|e| anyhow!("{e}"))?
        .ok_or_else(|| {
            anyhow!(
                "{verb} requires REPO_LINK_GITHUB_TOKEN or GITHUB_TOKEN to be set, \
                 or a token file at {} (write one with `rl gh auth`)",
                cfg.token_file_path.display()
            )
        })
}

/// Build a [`SyncService`] wired to a GitHub provider for the current
/// config. `verb` is interpolated into the "no token" error so a missing
/// token reports against the actual verb the user typed (`sync push`,
/// `task link`, `task claim`, …).
fn build_sync_service(cfg: &RepoLinkConfig, svc: &Services, verb: &str) -> Result<SyncService> {
    let token = require_github_token(cfg, verb)?;
    let provider: Arc<dyn ports::RemoteTaskProvider> =
        Arc::new(build_github_provider(&token, cfg).map_err(|e| anyhow!("{e}"))?);
    Ok(SyncService::new(
        svc.tasks_repo.clone(),
        svc.bindings_repo.clone(),
        provider,
    ))
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

/// Render the two-line file body: token on line 1, optional cached GitHub
/// login on line 2. Single-line files (login = None) keep parsing through
/// `infra_config::resolve_github_token` exactly as before — the second line
/// is purely additive.
fn render_token_file_body(token: &str, login: Option<&str>) -> String {
    match login {
        Some(l) => format!("{token}\n{l}\n"),
        None => token.to_string(),
    }
}

#[cfg(unix)]
fn write_token_file(path: &std::path::Path, token: &str, login: Option<&str>) -> Result<()> {
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
    file.write_all(render_token_file_body(token, login).as_bytes())
        .map_err(|e| anyhow!("failed to write token: {e}"))?;
    drop(file);

    // Re-assert permissions in case the file pre-existed with looser bits.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(|e| anyhow!("failed to set permissions: {e}"))?;

    Ok(())
}

#[cfg(not(unix))]
fn write_token_file(path: &std::path::Path, token: &str, login: Option<&str>) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow!("failed to create config dir: {e}"))?;
        }
    }
    std::fs::write(path, render_token_file_body(token, login))
        .map_err(|e| anyhow!("failed to write token file: {e}"))?;
    Ok(())
}

async fn workspace_dispatch(cmd: WorkspaceCmd, svc: &Services) -> Result<()> {
    match cmd {
        WorkspaceCmd::Create {
            name,
            description,
            local_only,
            project,
        } => {
            let dto = svc
                .workspaces
                .create(CreateWorkspaceCmd {
                    name,
                    description,
                    local_only,
                    project_spec: project,
                })
                .await?;
            render::workspace(&dto);
        }
        WorkspaceCmd::SetProject {
            workspace,
            project,
            none,
        } => {
            if !none && project.is_none() {
                return Err(anyhow!(
                    "rl workspace set-project requires either --project <spec> or --none"
                ));
            }
            let spec = if none { None } else { project.as_deref() };
            let dto = svc.workspaces.set_project(&workspace, spec).await?;
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
        WorkspaceCmd::Activate { id } => render::workspace(&svc.workspaces.activate(&id).await?),
        WorkspaceCmd::Pause { id } => render::workspace(&svc.workspaces.pause(&id).await?),
        WorkspaceCmd::Archive { id } => render::workspace(&svc.workspaces.archive(&id).await?),
    }
    Ok(())
}

/// Print a JSON ambiguous-handle error to stderr and exit with code 2.
/// Used by any resolver command when `ServiceError::AmbiguousHandle` fires.
fn handle_ambiguous(
    query: String,
    candidates: Vec<application_workspace::AmbiguousCandidate>,
) -> ! {
    let body = serde_json::json!({
        "error": "ambiguous",
        "query": query,
        "candidates": candidates,
    });
    eprintln!("{body}");
    std::process::exit(2);
}

async fn repo_dispatch(cmd: RepoCmd, svc: &Services) -> Result<()> {
    match cmd {
        RepoCmd::Attach {
            ws: WorkspaceArg { workspace },
            url,
            canonical,
            br: BranchArg { branch },
            path,
            no_link,
            prefix,
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
                    prefix,
                })
                .await?;
            render::attach_outcome(&outcome);
        }
        RepoCmd::Detach { id } => {
            let resolved = resolve_repo_handle_required(svc, &id).await?;
            svc.bindings.detach(&resolved).await?;
            println!("{}", serde_json::json!({ "detached": resolved }));
        }
        RepoCmd::List {
            ws: WorkspaceArg { workspace },
        } => render::repos(&svc.bindings.list(&workspace).await?),
        RepoCmd::Show { id } => match svc.bindings.show(&id).await {
            Ok(dto) => render::repo(&dto),
            Err(application_workspace::ServiceError::AmbiguousHandle { query, candidates }) => {
                handle_ambiguous(query, candidates);
            }
            Err(e) => return Err(anyhow!("{e}")),
        },
        RepoCmd::Rename { repo, name } => match svc.bindings.rename(&repo, name).await {
            Ok(dto) => render::repo(&dto),
            Err(application_workspace::ServiceError::AmbiguousHandle { query, candidates }) => {
                handle_ambiguous(query, candidates);
            }
            Err(e) => return Err(anyhow!("{e}")),
        },
        RepoCmd::SetPrefix { repo, prefix } => match svc.bindings.set_prefix(&repo, prefix).await {
            Ok(dto) => render::repo(&dto),
            Err(application_workspace::ServiceError::AmbiguousHandle { query, candidates }) => {
                handle_ambiguous(query, candidates);
            }
            Err(e) => return Err(anyhow!("{e}")),
        },
        RepoCmd::Alias(RepoAliasCmd::Add {
            repo,
            a: AliasArg { alias },
        }) => match svc.bindings.add_alias(&repo, alias).await {
            Ok(dto) => render::repo(&dto),
            Err(application_workspace::ServiceError::AmbiguousHandle { query, candidates }) => {
                handle_ambiguous(query, candidates);
            }
            Err(e) => return Err(anyhow!("{e}")),
        },
        RepoCmd::Alias(RepoAliasCmd::Rm {
            repo,
            a: AliasArg { alias },
        }) => match svc.bindings.remove_alias(&repo, &alias).await {
            Ok(dto) => render::repo(&dto),
            Err(application_workspace::ServiceError::AmbiguousHandle { query, candidates }) => {
                handle_ambiguous(query, candidates);
            }
            Err(e) => return Err(anyhow!("{e}")),
        },
        RepoCmd::Find { query } => render::find(&svc.bindings.find(&query).await?),
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
            let abs = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
            let query_path = abs.display().to_string();

            // Only "not a git repo" (or "git repo with no origin") maps to
            // null — those are legitimate no-matches. Any other error (git
            // binary missing, I/O failure, permission denied) is a real
            // problem worth surfacing so callers can distinguish broken
            // tooling from an unmapped path.
            let canonical_url = match discover_canonical(&abs) {
                Err(infra_git::GitError::NotARepo(_)) | Ok(None) => None,
                Err(e) => return Err(anyhow!("{e}")),
                Ok(Some(c)) => Some(c),
            };

            let matches = match canonical_url.as_deref() {
                Some(c) => svc.bindings.memberships_for_canonical_url(c).await?,
                None => vec![],
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
        Err(e) => Err(anyhow!("{e}")),
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

/// Best-effort canonical form of `input` for looking up a stored worktree.
///
/// If `canonicalize` succeeds outright, use it. Otherwise walk up the path
/// to the longest *existing* prefix, canonicalise that (so any symlinked
/// component gets resolved), and rejoin the missing tail components. This
/// makes `unlink` match `link`-stored entries even after the target leaf
/// has been deleted, including the macOS `/var → /private/var` case.
///
/// Last-resort fallback: convert to absolute via cwd for relative inputs,
/// or pass the raw string through if even that fails.
fn canonicalize_for_lookup(input: &str) -> String {
    let raw = PathBuf::from(input);

    if let Ok(p) = std::fs::canonicalize(&raw) {
        return p.display().to_string();
    }

    // Pop components until we find a prefix that canonicalises. The popped
    // pieces get rejoined to that resolved prefix to reconstruct the full
    // intended path.
    let mut prefix = raw.clone();
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    while let Some(name) = prefix.file_name().map(|n| n.to_owned()) {
        if !prefix.pop() || prefix.as_os_str().is_empty() {
            break;
        }
        suffix.push(name);
        if let Ok(canonical) = std::fs::canonicalize(&prefix) {
            let mut result = canonical;
            for piece in suffix.iter().rev() {
                result.push(piece);
            }
            return result.display().to_string();
        }
    }

    // Nothing in the path existed. For relative inputs, anchor to cwd so
    // we at least produce an absolute string the service can compare.
    if raw.is_absolute() {
        raw.display().to_string()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&raw).display().to_string())
            .unwrap_or_else(|_| input.to_string())
    }
}

#[derive(serde::Serialize)]
pub struct DiscoveredRepo {
    pub path: String,
    pub canonical: Option<String>,
}

async fn worktree_dispatch(cmd: WorktreeCmd, svc: &Services) -> Result<()> {
    match cmd {
        WorktreeCmd::Link {
            repo,
            path,
            br: BranchArg { branch },
        } => {
            let raw_path = std::path::Path::new(&path);
            let abs_path =
                std::fs::canonicalize(raw_path).unwrap_or_else(|_| raw_path.to_path_buf());

            let discovered = match discover_canonical(&abs_path) {
                Err(infra_git::GitError::NotARepo(_)) => {
                    anyhow::bail!("path is not a git repo: {}", abs_path.display());
                }
                Err(e) => return Err(anyhow!("{e}")),
                Ok(None) => {
                    anyhow::bail!("git repo at {} has no `origin` remote", abs_path.display());
                }
                Ok(Some(c)) => c,
            };

            // Route through the same resolver as `rl repo show`: a prefix /
            // name / alias works wherever a UUID does. Ambiguous handles exit
            // 2 with the candidate JSON rather than collapsing into a generic
            // error from the `?`.
            let binding = match svc.bindings.show(&repo).await {
                Ok(b) => b,
                Err(application_workspace::ServiceError::AmbiguousHandle { query, candidates }) => {
                    handle_ambiguous(query, candidates)
                }
                Err(e) => return Err(anyhow!("{e}")),
            };
            if discovered != binding.canonical_url {
                // Surface every binding that matches the discovered canonical so
                // the user can pick the right `--repo`. Picking arbitrarily (e.g.
                // `.first()`) misleads when the canonical is bound in multiple
                // workspaces.
                let memberships = svc
                    .bindings
                    .memberships_for_canonical_url(&discovered)
                    .await?;
                let repo_short = &repo;
                match memberships.as_slice() {
                    [] => anyhow::bail!(
                        "path origin '{discovered}' doesn't match repo {repo_short} \
                         ('{}') and no binding matches '{discovered}'; \
                         run `rl repo attach` first",
                        binding.canonical_url
                    ),
                    [only] => anyhow::bail!(
                        "path origin '{discovered}' doesn't match repo {repo_short} \
                         ('{}'); use --repo {} instead",
                        binding.canonical_url,
                        only.binding.id
                    ),
                    many => {
                        let candidates = many
                            .iter()
                            .map(|m| format!("{} (workspace: {})", m.binding.id, m.workspace.name))
                            .collect::<Vec<_>>()
                            .join(", ");
                        anyhow::bail!(
                            "path origin '{discovered}' doesn't match repo {repo_short} \
                             ('{}'); canonical '{discovered}' is bound in multiple workspaces: \
                             {candidates}; choose --repo explicitly",
                            binding.canonical_url
                        );
                    }
                }
            }

            let dto = svc
                .bindings
                .link_worktree(LinkWorktreeCmd {
                    repo_id: binding.id,
                    path: abs_path.display().to_string(),
                    branch,
                })
                .await?;
            render::repo(&dto);
        }
        WorktreeCmd::Unlink { repo, path } => {
            let resolved = resolve_repo_handle_required(svc, &repo).await?;
            // Mirror link's canonicalisation so identical --path input
            // round-trips. When the leaf is gone we still try to resolve
            // any symlinked prefix so e.g. macOS `/var/...` matches the
            // stored `/private/var/...`.
            let canonical_path = canonicalize_for_lookup(&path);
            let dto = svc
                .bindings
                .unlink_worktree(UnlinkWorktreeCmd {
                    repo_id: resolved,
                    path: canonical_path,
                })
                .await?;
            render::repo(&dto);
        }
        WorktreeCmd::PruneMissing { repo } => {
            let resolved = resolve_repo_handle_required(svc, &repo).await?;
            let dto = svc.bindings.prune_missing(&resolved).await?;
            render::repo(&dto);
        }
        WorktreeCmd::Reconcile {
            ws: WorkspaceArg { workspace },
            prune,
        } => {
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

/// Resolve a `--repo` argument (UUID / prefix / name / alias) to a binding
/// UUID, reusing the same resolver as `rl repo show`. `None` stays `None`.
/// Keeps `task create`/`edit` consistent with every other repo-addressing
/// command instead of demanding a raw UUID.
async fn resolve_repo_handle(svc: &Services, repo: Option<String>) -> Result<Option<String>> {
    match repo {
        Some(handle) => resolve_repo_handle_required(svc, &handle).await.map(Some),
        None => Ok(None),
    }
}

/// Required-arg sibling of `resolve_repo_handle`. Every command that takes a
/// repo positionally or via a non-optional `--repo` resolves through here so
/// a prefix / name / alias works in the same places a UUID does. Ambiguous
/// matches exit 2 with the same candidate JSON as `rl repo show`.
async fn resolve_repo_handle_required(svc: &Services, handle: &str) -> Result<String> {
    match svc.bindings.show(handle).await {
        Ok(dto) => Ok(dto.id),
        Err(application_workspace::ServiceError::AmbiguousHandle { query, candidates }) => {
            handle_ambiguous(query, candidates)
        }
        Err(e) => Err(anyhow!("{e}")),
    }
}

async fn task_dispatch(cmd: TaskCmd, svc: &Services, cfg: &RepoLinkConfig) -> Result<()> {
    match cmd {
        TaskCmd::Create {
            ws: WorkspaceArg { workspace },
            repo,
            title,
            body,
            priority,
        } => {
            let dto = svc
                .tasks
                .create(CreateTaskCmd {
                    workspace_id: workspace,
                    repo_id: resolve_repo_handle(svc, repo).await?,
                    title,
                    body,
                    priority,
                })
                .await?;
            render::task(&dto);
        }
        TaskCmd::Show { id } => render::task(&svc.tasks.show(&id).await?),
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

async fn query_dispatch(cmd: QueryCmd, svc: &Services, cfg: &RepoLinkConfig) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::parse_issue_url;

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
