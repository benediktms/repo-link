# repo-link

A local-first workspace and task manager that mirrors to GitHub Issues.

`repo-link` (CLI: `rl`) is a Rust workspace that lets you manage tasks across
many GitHub repos from a single local SQLite store. Tasks live locally first;
when you're ready to share them, `rl sync promote` creates the matching GitHub
issue and keeps the two in step. Snapshot history is recorded for every change,
so edits are auditable and rollback is safe.

Two binaries ship from the workspace:

- `rl` — the interactive CLI.
- `rld` — a background daemon that handles scheduled syncs and drift detection.

## Install

Build from source — this is a Cargo workspace, edition 2024:

```bash
git clone git@github.com:benediktms/repo-link.git
cd repo-link
just install
```

`just install` builds the release binaries, symlinks `rl` and `rld` into
`~/.local/bin/`, and registers the daemon (launchd on macOS, systemd on Linux).
Make sure `~/.local/bin` is on your `PATH`. From that point on, use the bare
`rl` command anywhere — it's cwd-independent.

To uninstall: `just uninstall`.

## Quickstart

```bash
# 1. Create a workspace (the local container for tasks + repo bindings).
rl workspace create my-workspace

# Grab the workspace UUID from the JSON output. Most commands take -w <id>.
WS=<workspace-id>

# 2. Attach a GitHub repo to the workspace.
rl repo attach \
  -w $WS \
  --url        git@github.com:you/your-repo.git \
  --canonical  github.com/you/your-repo

# 3. Create a task. --repo accepts the prefix, name, alias, or UUID.
rl task create -w $WS --repo your-repo --title "Add a feature" --priority p2

# 4. Stage it for sync, then promote to a GitHub issue.
rl task stage <task-id>
rl sync promote <task-id>

# 5. Later edits push back to the issue:
rl task edit  <task-id> --body "Updated body"
rl sync push  <task-id>
```

Every `rl` command emits JSON on stdout — pipe through `jq` for human-friendly
views, or use the dedicated query commands:

```bash
rl query ready    -w $WS    # next actionable task (accounts for blockers)
rl query mine     -w $WS    # your assigned tasks
rl query unsynced -w $WS    # local changes not yet pushed to GitHub
```

## Concepts

- **Workspace** — a named container for tasks and attached repos, backed by a
  local SQLite database.
- **Repo binding** — a GitHub repo attached to a workspace, with a short
  cosmetic **prefix** (e.g. `rpl`) and optional human-friendly **aliases**.
  The prefix doubles as a globally-unique repo locator.
- **Logical vs filing repo** — a task's **logical repo** (`repo_id`) owns its
  worktrees, prefix, and friendly-ID identity; its **filing repo** is where the
  backing GitHub issue is created. Usually identical, but a workspace can file
  issues in a dedicated issues-repo (`rl workspace set-filing-repo`). The filing
  repo is resolved and recorded at promote; only `repo_id` is exposed on task
  JSON. See [`0002-task-repo-axes.md`](docs/rfcs/0002-task-repo-axes.md).
- **Worktree link** — a filesystem path linked to a repo binding so `rl`
  commands can resolve workspace context from `cwd`.
- **Task** — a unit of work owned by a workspace. Its `id` renders as a
  composite **friendly ID** (`prefix-hash`, e.g. `rpl-ev6`) — but the
  underlying identity is a UUID, and the bare hash alone resolves globally.
- **Snapshot history** — every edit appends a new versioned row. `rl task
  snapshots <id>` shows the timeline; `rl task rollback <id> --to-version N`
  restores it.
- **Two orthogonal state axes** — `TaskStatus`
  (`open`/`in-progress`/`blocked`/`done`/`archived`) is the local lifecycle;
  `SyncState` (`local_only`/`staged`/`synced`/...) tracks remote alignment.
  GitHub mirrors this with two axes of its own — REST `open`/`closed` *and*
  the Projects v2 status field. Reconciling either side means handling both.

## Architecture

The workspace follows clean-architecture layering:

- `app-cli` / `app-daemon` — binary composition roots.
- `application-*` — use-case orchestration; depends on `ports` + `domain-*`.
- `domain-*` — pure business rules; depend only on `domain-core`.
- `ports` — trait contracts implemented by `infra-*` adapters.
- `infra-*` — adapters: `infra-sqlite` (SQLx), `infra-github` (octocrab), etc.
- `dto-shared` / `dto-events` — value types crossing layer or process
  boundaries.

Domain crates never depend on application, infra, or DTO crates. Application
crates never depend on infra crates directly — only on `ports` traits.

## Where to look next

- [`docs/rfcs/`](docs/rfcs/) — design rationale and accepted proposals,
  starting with [`0001-project-sync.md`](docs/rfcs/0001-project-sync.md).
- `AGENTS.md` — exhaustive command reference and agent-facing playbook,
  generated from the CLI's clap definitions. It's **gitignored** and
  regenerated per-checkout via `rl agents docs`; don't expect to find it on
  GitHub.
- `rl <subcommand> --help` is always the authoritative flag reference.

## License

TBD.
