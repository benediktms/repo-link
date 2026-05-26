`rl` (repo-link) is a local-first workspace and task manager that syncs to GitHub Issues.

Core concepts:

- **Workspace** — a named container for tasks and attached repos. Lives in a local SQLite database.
- **Repo binding** — an attachment of a GitHub repo to a workspace, with an optional human-friendly name and aliases.
- **Worktree link** — a filesystem path linked to a repo binding so `rl` can resolve commands by `cwd`.
- **Task** — a unit of work owned by a workspace. Tracks `status` and `sync_state` (such as open/closed and local_only/staged/synced respectively; see the CLI `--status` and `--sync-state` flags for the complete set of values).
- **Snapshot history** — every change to a task is recorded; `sync` operations promote / push / pull against GitHub Issues.

All commands emit JSON on stdout; pipe through `jq` for human-friendly views. Run `rl <subcommand> --help` (or `rl <subcommand> <verb> --help`) for the authoritative flag reference of any command — the workflow snippets below show the common path, not every option.

## Working with `rl` as an agent

The "This repo" block below names the workspace(s) this checkout belongs to. Use the `workspace_id` from there as `--workspace <id>` in every command. If the block says `status: unbound`, follow its hint to attach the repo before doing anything else.

If you only know your `cwd` and need to recover workspace context mid-session, run:

```bash
rl repo locate --path .
```

It returns every workspace this checkout is bound to (a repo can live in more than one).

Most hot flags have single-letter short forms (e.g. `-w` for `--workspace`). The examples below use the long forms for clarity; run `rl <subcommand> --help` to see the shorts for any specific command.

### Finding work

Ask `rl` what's actionable before reading the issue tracker or guessing from git history:

```bash
rl query ready --workspace <id>     # open tasks, not transitively blocked, sorted by priority
rl query mine  --workspace <id>     # narrows ready work to tasks assigned to you
rl query overview --workspace <id>  # counts by status / sync_state — useful sanity check
```

`query ready` is the canonical "what's next" answer. The first entry is the highest-priority unblocked task. Prefer it over `gh issue list`, because it also accounts for transitive blockers and local-only tasks that haven't been pushed to GitHub yet.

### Before you start: check drift

Local task state and remote GitHub Issues can diverge — someone edits an issue on the web, a CI job closes one, etc. Always run drift detection before changing or completing work:

```bash
rl query drift    --workspace <id>  # tasks whose local snapshot disagrees with the remote
rl query unsynced --workspace <id>  # local_only / staged / DirtyLocal tasks not yet pushed
```

If drift is non-empty, reconcile with `rl sync pull <task-id>` before editing the task; otherwise your changes will collide with the remote.

### Starting work on a task

```bash
rl task show  <task-id>             # full snapshot, including relations + remote ref
rl task start <task-id>             # Open|Blocked → InProgress
```

`task start` is what moves a task into `InProgress`; do this once per task at the beginning of a session so other queries (`query ready`, `query mine`) reflect reality.

### Before you stop: sync your work

When a session is wrapping up, never leave local work unpushed. The flow is:

```bash
rl query unsynced --workspace <id>           # see what's pending
rl sync promote   <task-id>                  # Draft/Staged → create remote issue
rl sync push      <task-id>                  # DirtyLocal → push local edits to remote
rl task complete  <task-id>                  # mark done locally (then push if needed)
```

Then re-run `rl query unsynced --workspace <id>` and confirm it returns an empty list. If it doesn't, the session isn't done — either push the remaining tasks or note them as deliberate carry-over for the next session.

### Useful filters and views

```bash
rl query blocked      --workspace <id>   # tasks in Blocked state
rl query stale        --workspace <id>   # InProgress tasks untouched for a while
rl query contributors --workspace <id>   # who has touched which tasks
rl task snapshots <task-id>              # full version history of one task
```
