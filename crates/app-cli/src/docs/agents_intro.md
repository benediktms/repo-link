`rl` (repo-link) is a local-first workspace and task manager that syncs to GitHub Issues.

Core concepts:

- **Workspace** — a named container for tasks and attached repos. Lives in a local SQLite database.
- **Repo binding** — an attachment of a GitHub repo to a workspace, with an optional human-friendly name and aliases.
- **Worktree link** — a filesystem path linked to a repo binding so `rl` can resolve commands by `cwd`.
- **Task** — a unit of work owned by a workspace. Tracks `status` and `sync_state` (such as open/closed and local_only/staged/synced respectively; see the CLI `--status` and `--sync-state` flags for the complete set of values).
- **Snapshot history** — every change to a task is recorded; `sync` operations promote / push / pull against GitHub Issues.

All commands emit JSON on stdout; pipe through `jq` for human-friendly views. Run `rl <subcommand> --help` for the full reference of any command below.
