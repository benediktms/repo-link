//! The clap command tree — declarations only. `Cli` is the root parser; the
//! `Cmd` enum and every `*Cmd` subcommand enum live here, along with the
//! shared `#[command(flatten)]` arg groups and the value-parser fns. The
//! dispatch modules name these as `crate::cli::*`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::daemon;

#[derive(Parser, Debug)]
#[command(
    name = "repo-link",
    version,
    about = "Local-first workspace + task manager. All output is JSON; pipe through `jq` for human-friendly views."
)]
pub(crate) struct Cli {
    /// SQLite database path. Falls back to platform data dir.
    #[arg(long, env = "REPO_LINK_DB", global = true)]
    pub(crate) db: Option<PathBuf>,

    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

// Shared `#[command(flatten)]` arg groups. One definition per concept,
// reused by every variant that needs it — short/long mapping, help text,
// and any future env var or alias live in exactly one place.

#[derive(Args, Debug)]
pub(crate) struct WorkspaceArg {
    /// Workspace UUID. Optional: when omitted, it is derived from the current
    /// directory's repo (its git origin → the workspace that has that repo
    /// attached). Ambiguous (repo in >1 workspace) or no-match cwd errors and
    /// asks for `--workspace`.
    #[arg(short = 'w', long)]
    pub(crate) workspace: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct TaskArg {
    /// Task reference: UUID, bare hash, or `prefix-hash`.
    #[arg(short = 't', long)]
    pub(crate) task: String,
}

#[derive(Args, Debug)]
pub(crate) struct BranchArg {
    /// Tracked branch.
    #[arg(short = 'b', long)]
    pub(crate) branch: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct AliasArg {
    /// Alias string.
    #[arg(short = 'a', long)]
    pub(crate) alias: String,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Cmd {
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
pub(crate) enum WorkspaceCmd {
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
        /// Include archived workspaces, which are hidden from the listing by
        /// default.
        #[arg(short = 'a', long)]
        include_archived: bool,
    },
    Show {
        id: String,
    },
    /// Edit a workspace's mutable display fields.
    ///
    /// At least one of `--name` or `--description` must be supplied.
    Edit {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(short = 'd', long)]
        description: Option<String>,
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
    /// Bring an archived workspace back to Active — the inverse of `archive`,
    /// and the only way out of the otherwise-terminal Archived state.
    Unarchive {
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
    /// Set (or clear) the workspace's default filing repo — where a task's
    /// backing GitHub issue is filed when no per-task override applies
    /// (RFC 0002 §4 / D2 step-2). The final home for this setting is
    /// `repo-link.toml` (GitHub #91, blocked by the epic); this verb is
    /// the interim CLI surface.
    ///
    /// `<repo>` resolves the same way `--repo` does: UUID, short prefix,
    /// name, or alias. Ambiguous matches exit 2 with a candidate list.
    /// Reassigning an already-set default is permitted (forward-looking;
    /// per-task `filing_repo_id` values are never retargeted).
    SetFilingRepo {
        workspace: String,
        /// Repo binding handle (UUID / prefix / name / alias).
        /// Mutually exclusive with `--none`.
        #[arg(long, conflicts_with = "none")]
        repo: Option<String>,
        /// Clear the workspace filing-repo default. Mutually exclusive with
        /// `--repo`.
        #[arg(long)]
        none: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum RepoCmd {
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
        /// Include archived workspaces in the matches, hidden by default.
        #[arg(short = 'a', long)]
        include_archived: bool,
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
    /// Inspect (and optionally repair) tasks whose recorded
    /// `filing_repo_id` references a deleted binding (rpl-sv2). Without
    /// `--repair`: list each affected task with the auto-resolved
    /// target — the user audits before committing. With `--repair`:
    /// re-point every affected task's `filing_repo_id` to the target
    /// and tag the resulting snapshot with `FilingRepoRepair`. The
    /// `--target <handle>` override forces every affected task to be
    /// re-pointed at that specific binding, skipping the auto-target
    /// chain. Run after a GitHub org-move to clean up the silent
    /// divergence the unfix-up leaves behind.
    Doctor {
        #[command(flatten)]
        ws: WorkspaceArg,
        /// Apply the re-point. Without this flag, the command is
        /// read-only and emits a list of affected tasks.
        #[arg(long)]
        repair: bool,
        /// Force every affected task to be re-pointed at this binding
        /// (UUID / prefix / name / alias, same forms as `rl repo show`).
        /// Skips the auto-target chain.
        #[arg(long)]
        target: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum RepoAliasCmd {
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
pub(crate) enum WorktreeCmd {
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
pub(crate) enum TaskCmd {
    Create {
        #[command(flatten)]
        ws: WorkspaceArg,
        /// Logical repo binding — where the code/worktrees live and the source
        /// of the task's ID prefix. Today the issue is also filed in this repo
        /// on promote (logical == filing repo until RFC 0002). By UUID / prefix
        /// / name / alias (same forms as `rl repo show`).
        #[arg(short = 'r', long)]
        repo: Option<String>,
        /// Per-task filing-repo override (RFC 0002 D2 step 1, #122). Accepts
        /// the same handle forms as `--repo` (UUID / prefix / name / alias).
        /// When present, the resolved binding beats the workspace filing default
        /// and the logical repo in the D2 resolution chain.
        ///
        /// Note: `rl task create` only mints a local draft — it does not
        /// promote the task to a remote issue. The filing-repo override is
        /// consumed at the first-filing transition (`rl sync promote`), which
        /// is not yet wired to read a per-task pending override. Supplying this
        /// flag on a non-promoting create is therefore rejected with a deferral
        /// error; use `rl sync promote` to file the task and control the target
        /// repo via the workspace filing default for now.
        #[arg(long = "filing-repo")]
        filing_repo: Option<String>,
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        priority: Option<String>,
    },
    Show {
        id: String,
        /// Opt in to a network fetch: observe the remote and refresh the
        /// "last refreshed" stamp before rendering (RFC 0004 D4). Default
        /// `show` is offline. A fetch failure is non-fatal — the cached value
        /// is rendered with a `last_refresh_failed` annotation. Does NOT
        /// reconcile content (use `rl sync pull` for that).
        #[arg(long)]
        refresh: bool,
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
        /// Reassign the task's logical repo binding (code/worktrees/prefix), by
        /// UUID / prefix / name / alias (same forms as `rl repo show`). Use
        /// this to attach a repo to a task created without one — required
        /// before `sync promote`, which needs a logical repo to know which
        /// GitHub repo to open the issue in (the logical repo is also the
        /// filing repo today, until RFC 0002). Only valid while the task is not
        /// yet synced to a remote issue; reassigning a synced task is rejected.
        #[arg(short = 'r', long)]
        repo: Option<String>,
    },
    List {
        #[arg(short = 'w', long)]
        workspace: Option<String>,
        /// Filter by lifecycle status (`open` / `closed` / `all`). Defaults to
        /// `open` — pass `all` to include completed and dropped tasks.
        #[arg(short = 's', long)]
        status: Option<String>,
        /// Filter by sync state (`local_only` / `staged` / `synced` / `dirty_local` / `dirty_remote` / `conflict`).
        #[arg(long)]
        sync_state: Option<String>,
    },
    /// Stage one or more tasks for sync.
    Stage {
        #[arg(required = true)]
        tasks: Vec<String>,
    },
    /// Assert the task is open (no-op if already open).
    ///
    /// Ensures the task is in the open state so your local queries
    /// (`query ready`, `query mine`) reflect reality. No-op if the task is
    /// already open; errors if the task is closed (reopen it first). Does NOT
    /// touch `assignees` and does NOT push to GitHub — teammates won't see
    /// anything change. Works on purely-local tasks. Offline-safe. Use
    /// `task claim` instead when you want to announce externally that you've
    /// picked up the task.
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
    /// 2. Assert the task is open (no-op if already open).
    /// 3. Best-effort `sync push` to mirror the new state to the remote
    ///    issue. Local-only / staged tasks skip the push with a hint to
    ///    promote first.
    ///
    /// Refuses on closed tasks (reopen first). Requires the cached GitHub login
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
    Comment { id: String, body: String },
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
    /// Relate two tasks — the reciprocal edge is added to `--other`
    /// automatically (e.g. `blocks` ⇒ `blocked_by` on the other task).
    /// Self-relations and cycles in `blocked_by`/`parent_of` are rejected.
    ///
    /// Pass `--remove` to delete instead: with `--kind`+`--other` it drops
    /// that one edge (and its reciprocal); with neither it drops ALL
    /// relations on the task.
    Relate {
        id: String,
        #[arg(long)]
        kind: Option<RelationKindArg>,
        #[arg(long)]
        other: Option<String>,
        #[arg(long, short = 'r')]
        remove: bool,
    },
    /// List the full snapshot history for a task.
    Snapshots { id: String },
    /// Roll a task back to a historical snapshot version.
    Rollback {
        id: String,
        #[arg(long)]
        to_version: u64,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum QueryCmd {
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
    /// Completion rollup of a parent task's children (done/total + per-child
    /// detail). Accepts a UUID, bare hash, or `prefix-hash` composite.
    Children { id: String },
}

#[derive(Subcommand, Debug)]
pub(crate) enum SyncCmd {
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
    /// Show dead-lettered outbox entries — outbound mutations that exhausted
    /// their retries and were permanently parked (RFC 0001 Stage 6, #54).
    /// Local read; no GitHub token required.
    Outbox,
}

#[derive(Subcommand, Debug)]
pub(crate) enum GhCmd {
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
pub(crate) enum AgentsCmd {
    /// Render a self-documenting `rl` block into `./AGENTS.md`.
    ///
    /// Splices between `<!-- rl:doc:start -->` and `<!-- rl:doc:end -->`,
    /// creating the file if missing or appending the block if no markers
    /// are present. Always rewrites the block on every run.
    Docs,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ProjectCmd {
    /// Link a project by fetching its schema from GitHub. `<target>` is
    /// `owner/number` (e.g. `benediktms/3`). The Status field and its option
    /// catalog are read over GraphQL, and the local-status → option mapping
    /// is auto-derived by option name (refine it later with `rl project map`).
    /// Requires a GitHub token (see `rl gh auth`).
    Link {
        /// The project to link, as `owner/number` (e.g. `benediktms/3`).
        target: String,
    },
    /// List every locally-known project (across all workspaces).
    List,
    /// Show one project. `<spec>` is `owner/number` or a `PVT_…` node id.
    Show { spec: String },
    /// Set a local TaskStatus → project option mapping.
    Map {
        spec: String,
        /// Local task status (`open` / `closed`).
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

/// CLI surface for `domain_task::RelationKind`. Kept as a clap-local mirror so
/// the domain crate stays free of a clap dependency. The `value(name = …)`
/// tokens are the canonical `snake_case` strings the application layer parses
/// back into `RelationKind`, so the JSON `kind` echoes the input verbatim.
///
/// `depends_on` is intentionally absent — it was dropped as a redundant
/// synonym of `blocked_by`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum RelationKindArg {
    #[value(name = "blocked_by")]
    BlockedBy,
    #[value(name = "blocks")]
    Blocks,
    #[value(name = "duplicates")]
    Duplicates,
    #[value(name = "parent_of")]
    ParentOf,
    #[value(name = "child_of")]
    ChildOf,
    #[value(name = "related_to")]
    RelatedTo,
}

impl RelationKindArg {
    /// The canonical `snake_case` string accepted by the application layer's
    /// `parse_enum::<RelationKind>`.
    pub(crate) fn as_kind_str(self) -> &'static str {
        match self {
            RelationKindArg::BlockedBy => "blocked_by",
            RelationKindArg::Blocks => "blocks",
            RelationKindArg::Duplicates => "duplicates",
            RelationKindArg::ParentOf => "parent_of",
            RelationKindArg::ChildOf => "child_of",
            RelationKindArg::RelatedTo => "related_to",
        }
    }
}
