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
    /// Org-level GitHub resources (RFC 0006 D5) — currently just the native
    /// issue-type registry.
    #[command(subcommand)]
    Org(OrgCmd),
    /// Manage the background reconciliation daemon (launchd / systemd unit).
    #[command(subcommand)]
    Daemon(daemon::DaemonCmd),
    /// Resolve the current directory's full working context in one shot.
    ///
    /// Use this when an `rl` workflow needs the current checkout's workspace
    /// context instead of grepping AGENTS.md or guessing workspace ids. No
    /// arguments — always evaluates the cwd's git origin, and archived
    /// workspaces are excluded. Returns every workspace membership for this
    /// checkout, each with its repo binding, project + filing repo, and the
    /// sibling repo roster.
    Here,
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
    ///
    /// `<workspace>` is optional: when omitted, it is derived from the current
    /// directory's repo (its git origin → the workspace that has that repo
    /// attached), same as `--workspace` elsewhere.
    SetProject {
        workspace: Option<String>,
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
    ///
    /// `<workspace>` is optional: when omitted, it is derived from the current
    /// directory's repo (its git origin → the workspace that has that repo
    /// attached), same as `--workspace` elsewhere.
    SetFilingRepo {
        workspace: Option<String>,
        /// Repo binding handle (UUID / prefix / name / alias).
        /// Mutually exclusive with `--none`.
        #[arg(long, conflicts_with = "none")]
        repo: Option<String>,
        /// Clear the workspace filing-repo default. Mutually exclusive with
        /// `--repo`.
        #[arg(long)]
        none: bool,
    },
    /// Set (or clear) the workspace's default native issue-type NAMES
    /// (RFC 0006 #239 / §0 A4). When a task is first filed (`rl sync promote`)
    /// carrying no explicit `--type`, the effective type is derived from these:
    /// a sub-issue (a task with a `child_of` relation) uses `--sub-issue`, a
    /// free-standing task uses `--standalone`. Names are org-specific and
    /// resolved case-insensitively against the filing owner's issue-type
    /// registry at promote (an absent name degrades to a logged advisory, not
    /// an error). The default only ever *fills* a never-set type at first
    /// filing — it never overrides an explicit type or a later re-save.
    ///
    /// Merge semantics: an omitted flag leaves that default unchanged (setting
    /// one never wipes the other); `--none` clears both. Workspace-scoped
    /// because native Type works with no board.
    ///
    /// `<workspace>` is optional: when omitted, it is derived from the current
    /// directory's repo, same as `--workspace` elsewhere.
    SetDefaultType {
        workspace: Option<String>,
        /// Default type name for free-standing (non-sub-issue) tasks
        /// (e.g. `Story`). Omitting it leaves the current value untouched.
        #[arg(long, conflicts_with_all = ["none", "clear_standalone"])]
        standalone: Option<String>,
        /// Default type name for sub-issue (`child_of`) tasks (e.g. `Task`).
        /// Omitting it leaves the current value untouched.
        #[arg(long = "sub-issue", conflicts_with_all = ["none", "clear_sub_issue"])]
        sub_issue: Option<String>,
        /// Clear ONLY the standalone default (leaves the sub-issue default).
        #[arg(long = "clear-standalone", conflicts_with_all = ["none", "standalone"])]
        clear_standalone: bool,
        /// Clear ONLY the sub-issue default (leaves the standalone default).
        #[arg(long = "clear-sub-issue", conflicts_with_all = ["none", "sub_issue"])]
        clear_sub_issue: bool,
        /// Clear BOTH workspace default issue types. Mutually exclusive with the
        /// set/clear flags above.
        #[arg(long, conflicts_with_all = ["standalone", "sub_issue", "clear_standalone", "clear_sub_issue"])]
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
    /// List repository bindings across all active workspaces. Can run outside
    /// a bound checkout; use `--workspace` to optionally scope the results to
    /// one workspace.
    List {
        /// Optionally scope the results to one workspace UUID.
        #[arg(short = 'w', long)]
        workspace: Option<String>,
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
        /// Repo binding, by UUID / prefix / name / alias (same forms as
        /// `rl repo show`). Optional: when omitted, derived from the current
        /// directory's repo (cwd git origin → the bound checkout).
        #[arg(long)]
        repo: Option<String>,
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
        /// Repo binding, by UUID / prefix / name / alias (same forms as
        /// `rl repo show`). Optional: when omitted, derived from the current
        /// directory's repo (cwd git origin → the bound checkout).
        #[arg(long)]
        repo: Option<String>,
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
    /// `filing_repo_id` references a deleted binding. Without
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
        /// Repo binding, by UUID / prefix / name / alias (same forms as
        /// `rl repo show`). Optional: when omitted, derived from the current
        /// directory's repo (cwd git origin → the bound checkout).
        #[arg(long)]
        repo: Option<String>,
        #[command(flatten)]
        a: AliasArg,
    },
    Rm {
        /// Repo binding, by UUID / prefix / name / alias (same forms as
        /// `rl repo show`). Optional: when omitted, derived from the current
        /// directory's repo (cwd git origin → the bound checkout).
        #[arg(long)]
        repo: Option<String>,
        #[command(flatten)]
        a: AliasArg,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum WorktreeCmd {
    Link {
        /// Repo binding, by UUID / prefix / name / alias (same forms as
        /// `rl repo show`). Optional: when omitted, derived from the current
        /// directory's repo (cwd git origin → the bound checkout).
        #[arg(long)]
        repo: Option<String>,
        #[arg(short = 'p', long)]
        path: String,
        #[command(flatten)]
        br: BranchArg,
    },
    Unlink {
        /// Repo binding, by UUID / prefix / name / alias (same forms as
        /// `rl repo show`). Optional: when omitted, derived from the current
        /// directory's repo (cwd git origin → the bound checkout).
        #[arg(long)]
        repo: Option<String>,
        #[arg(short = 'p', long)]
        path: String,
    },
    PruneMissing {
        /// Repo binding, by UUID / prefix / name / alias (same forms as
        /// `rl repo show`). Optional: when omitted, derived from the current
        /// directory's repo (cwd git origin → the bound checkout).
        #[arg(long)]
        repo: Option<String>,
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

/// `rl task search` positional + flag surface (RFC 0007 D10).
#[derive(clap::Args, Debug)]
pub(crate) struct TaskSearchArgs {
    /// Search query: exact phrase, identifier/error string, or natural
    /// language. Shell quoting is removed before the CLI receives the value.
    pub query: String,
    #[command(flatten)]
    pub ws: WorkspaceArg,
    /// Restrict to a repo binding, by UUID / prefix / name / alias.
    #[arg(long)]
    pub repo: Option<String>,
    /// Filter by lifecycle status: `open` / `closed` / `all`. Default `all`.
    #[arg(short = 's', long)]
    pub status: Option<String>,
    /// Maximum number of results (default 10). `--limit 0` is rejected.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Force exact substring matching (overrides identifier classification).
    #[arg(long)]
    pub exact: bool,
}

/// `rl task search-index` subcommands (RFC 0007 D10).
#[derive(Subcommand, Debug)]
pub(crate) enum SearchIndexCmd {
    Status {},
    Rebuild {},
    Clear {},
    PrepareModel {},
}

#[derive(Subcommand, Debug)]
pub(crate) enum TaskCmd {
    Create {
        #[command(flatten)]
        ws: WorkspaceArg,
        /// Logical repo binding — where the code/worktrees live and the source
        /// of the task's ID prefix. Today the issue is also filed in this repo
        /// on promote (logical == filing repo until RFC 0002). By UUID / prefix
        /// / name / alias (same forms as `rl repo show`). Optional: at least one
        /// of `--workspace` / `--repo` must resolve and the other is inferred —
        /// the workspace from the repo's binding when `--repo` is given, or the
        /// repo from the cwd checkout (scoped to the workspace) when `--repo` is
        /// omitted. With neither, both come from cwd iff its repo is in exactly
        /// one active workspace.
        #[arg(short = 'r', long)]
        repo: Option<String>,
        /// Per-task filing-repo override (RFC 0002 D2 step 1, #122). Accepts
        /// the same handle forms as `--repo` (UUID / prefix / name / alias).
        /// When present, the resolved binding beats the workspace filing default
        /// and the logical repo in the D2 resolution chain.
        ///
        /// The override is recorded on the draft at create time and honoured at
        /// the first-filing transition: `rl sync promote` files the eventual
        /// GitHub issue in this repo, independent of the logical `--repo`.
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
    /// `--assignee`, `--clear-assignees`, `--repo`, `--filing-repo`,
    /// `--type`, or `--clear-type` must be supplied.
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
        /// entirely leaves the existing assignees untouched only when
        /// `--clear-assignees` is also omitted. Mutually
        /// exclusive with `--clear-assignees`.
        #[arg(long = "assignee", conflicts_with = "clear_assignees")]
        assignees: Vec<String>,
        /// Clear all assignees. Mutually exclusive with `--assignee`.
        #[arg(long = "clear-assignees", conflicts_with = "assignees")]
        clear_assignees: bool,
        /// Reassign the task's logical repo binding (code/worktrees/prefix), by
        /// UUID / prefix / name / alias (same forms as `rl repo show`). Use
        /// this to attach a logical repo to a task created without one. The
        /// filing repo is controlled separately by `--filing-repo`.
        #[arg(short = 'r', long)]
        repo: Option<String>,
        /// Set an unset per-task filing repo. Accepts the same UUID / prefix /
        /// name / alias handles as `--repo`. Changing or clearing a filing
        /// repo after it has been recorded remains rejected.
        #[arg(long = "filing-repo")]
        filing_repo: Option<String>,
        /// Set the task's local extensible issue type (RFC 0006 D7): a
        /// well-known name (`task` / `bug` / `feature`, case-insensitive) or
        /// any other string, kept verbatim as a custom type. If the task is a
        /// mirror, a real change is projected onto GitHub: if its board has a
        /// custom "Type"/"Types" single-select the value lands there by option
        /// name (#238, works on user-owned boards); otherwise onto the issue's
        /// native Type field via the org's issue-type registry (#228). An
        /// unmapped name or an unavailable field degrades to a logged advisory,
        /// not an error. Mutually exclusive with `--clear-type`.
        #[arg(long = "type", conflicts_with = "clear_type")]
        issue_type: Option<String>,
        /// Clear the task's local issue type. Mutually exclusive with
        /// `--type`.
        #[arg(long = "clear-type", conflicts_with = "issue_type")]
        clear_type: bool,
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
    /// Search current task content (RFC 0007): exact / identifier / natural
    /// retrieval over title, body, and comments. No model required.
    Search {
        #[command(flatten)]
        args: TaskSearchArgs,
    },
    /// Maintain the disposable task-search index (RFC 0007 D10).
    SearchIndex {
        #[command(subcommand)]
        cmd: SearchIndexCmd,
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
        /// Compare against the cached board status instead of reading the
        /// board live. Needs no token and makes no network call — the fast
        /// path, at the cost of being only as fresh as the last poll.
        ///
        /// Live is the default. Without a token, or if the read fails, the
        /// live mode degrades to this and says so in `messages`.
        #[arg(long)]
        offline: bool,
    },
    /// Tasks that are actionable now: open + not transitively blocked, grouped
    /// under their parent task. Defaults to the frontier of the workspaces the
    /// current repo is attached to (all workspaces when the cwd isn't a bound
    /// repo); `--workspace` filters to one, `--local` to the local repo.
    Ready {
        /// Workspace UUID. Omitted: every workspace the current repo is
        /// attached to (or all workspaces when the cwd isn't a bound repo).
        #[arg(short = 'w', long)]
        workspace: Option<String>,
        /// Only tasks belonging to the local repo (this checkout's own repo),
        /// narrowing the frontier from the workspaces it is attached to.
        /// Errors when the checkout isn't bound to a workspace.
        #[arg(long)]
        local: bool,
    },
    /// Open tasks assigned to a user. Defaults to the cached GitHub login
    /// (so it round-trips with `task claim`), then git config user.name,
    /// then $REPO_LINK_USER, then $USER.
    Mine {
        #[command(flatten)]
        ws: WorkspaceArg,
        // No `env = "REPO_LINK_USER"` here: binding the env var to the arg
        // makes clap pre-fill `assignee`, which collapses the explicit-flag
        // and env-var precedence steps into one and makes the git user.name
        // step unreachable whenever REPO_LINK_USER is set. The full chain is
        // resolved in `query_dispatch` instead.
        #[arg(long)]
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
        /// Force local-wins: push a task stuck in Conflict, treating the local
        /// content as the resolution (discards the remote divergence). Escape
        /// hatch for an unresolved manual merge.
        #[arg(long)]
        force: bool,
    },
    /// Pull the latest remote snapshot and reconcile.
    Pull {
        #[command(flatten)]
        t: TaskArg,
        /// Force remote-wins: on a manual-merge conflict, accept the remote and
        /// clear the conflict (discards the local divergence). Escape hatch for
        /// an unresolved manual merge.
        #[arg(long)]
        force: bool,
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
    /// List remote GitHub issues, marking each `tracked` (a local task already
    /// mirrors it) or `untracked` (an import candidate). Read-only; requires a
    /// GitHub token. Repo selection: `--repo` lists just that repo; otherwise,
    /// if the workspace has a filing default the output is grouped (filing repo
    /// first, then each bound canonical repo); otherwise the current
    /// directory's repo is listed.
    ListRemote {
        /// Repo to list, as UUID / prefix / name / alias. When omitted,
        /// resolution falls back to the workspace filing default (grouped) or
        /// the cwd checkout.
        #[arg(short = 'r', long)]
        repo: Option<String>,
        /// Only list issues updated at or after this instant (GitHub filters by
        /// `updatedAt`). Accepts `YYYY-MM-DD` or an RFC 3339 timestamp.
        /// Defaults to 90 days ago.
        #[arg(long)]
        since: Option<String>,
        #[command(flatten)]
        ws: WorkspaceArg,
    },
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
    /// Set a local Priority → project Priority option mapping.
    MapPriority {
        spec: String,
        /// Local priority (`p0` / `p1` / `p2` / `p3`).
        #[arg(long)]
        priority: String,
        /// Option ID on the project's Priority field.
        #[arg(long = "option-id")]
        option_id: String,
    },
    /// Unlink a project locally. Workspaces attached to it have their
    /// `project_id` reset to NULL via the storage cascade.
    Unlink { spec: String },
}

#[derive(Subcommand, Debug)]
pub(crate) enum OrgCmd {
    /// (Re)fetch and persist an org's native issue-type registry over
    /// GraphQL (RFC 0006 D5/D8) — the same op `rl project link` performs for
    /// the *project* owner, exposed here as a standalone trigger for any
    /// owner. Requires a GitHub token (see `rl gh auth`). Prints
    /// `{ owner, available, types }` to stdout; when the catalog is empty
    /// (a user account, or the org feature disabled), an advisory rides on
    /// stderr instead of failing the command.
    FetchIssueTypes {
        /// GitHub org/user login to (re)fetch. When omitted, it's derived
        /// from the current directory's GitHub git origin — errors if the
        /// cwd isn't a GitHub checkout with a resolvable origin.
        owner: Option<String>,
    },
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
