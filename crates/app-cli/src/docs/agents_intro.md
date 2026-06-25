`rl` (repo-link) is a local-first workspace and task manager that syncs to GitHub Issues.

Core concepts:

- **Workspace** — a named container for tasks and attached repos. Lives in a local SQLite database.
- **Repo binding** — an attachment of a GitHub repo to a workspace, with an optional human-friendly name and aliases.
- **Logical vs filing repo** — a task has two repo axes. Its **logical repo** (`repo_id`) is where the code/worktrees live and the source of the friendly-ID prefix; its **filing repo** is where the backing GitHub issue is actually created. They are usually the same, but a workspace can file all its issues in a dedicated issues-repo via `rl workspace set-filing-repo` (or per task via `rl task create --filing-repo`). The filing repo is resolved and recorded once, at promote (the first filing), and is immutable thereafter; resolution precedence is per-task `--filing-repo` > workspace filing default > logical `repo_id`. Only `repo_id` appears on task JSON — the filing repo is surfaced solely by `rl task show`, so your normal `repo_id` usage is unchanged. See `docs/rfcs/0002-task-repo-axes.md` for the full model.
- **Worktree link** — a filesystem path linked to a repo binding so `rl` can resolve commands by `cwd`.
- **Task** — a unit of work owned by a workspace. Tracks `status` and `sync_state` (such as open/closed and local_only/staged/synced respectively; see the CLI `--status` and `--sync-state` flags for the complete set of values). Each task carries a friendly composite ID of the form `prefix-hash` (e.g. `rpl-ev6`) in its `id` field — see "Friendly IDs" below.
- **Snapshot history** — every change to a task is recorded; `sync` operations promote / push / pull against GitHub Issues.

### Friendly IDs

Every task's `id` field normally renders as a composite `<prefix>-<hash>` (e.g. `rpl-ev6`), where the prefix is the binding's short cosmetic handle and the hash is a globally-unique random base32 string. UUIDs remain the on-disk identity; a task with no repo binding shows just the bare `<hash>`.

Anywhere a task ID is accepted, three forms resolve to the same task:

- `rl task show rpl-ev6` — full composite (preferred for humans; carries the repo context visually).
- `rl task show ev6` — bare hash; works because the hash alone is globally unique.
- `rl task show <uuid>` — the underlying UUID also resolves.

A mismatched prefix is a hard error: `rl task show wrong-ev6` will refuse rather than silently resolving by hash, since the mismatch usually indicates a stale copy-paste from another repo. The bare-hash form (`ev6`) sidesteps this.

The repo's prefix doubles as a globally-unique repo locator — `rl repo show rpl` works the same as `rl repo show <uuid>` or `rl repo show <name>`. The prefix is derived from the repo's name automatically at attach time; override with `rl repo attach --prefix <p>`, change later with `rl repo set-prefix --repo <id> --prefix <p>`.

All commands emit JSON on stdout. Use jq to extract or reshape fields; present results to a human as a markdown table, not raw JSON or a jq dump.
Run `rl <subcommand> --help` (or `rl <subcommand> <verb> --help`) for the authoritative flag reference of any command — the workflow snippets below show the common path, not every option.

## Working with `rl` as an agent

**Before doing anything else in a session** — before reading the issue tracker, running `gh issue list`, scanning open PRs, or guessing from git history — run `rl query ready --workspace <id>` (or `rl query mine --workspace <id>`). The first entry is the next task. `rl` accounts for transitive blockers and local-only tasks the issue tracker cannot see, so it is **strictly more informative** than the GitHub view. This is a rule, not a suggestion. (Get `<id>` from the "This repo" block below; the detailed reference is under "Finding work".)

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

Two wrinkles worth knowing when you present these results:

- `query ready` / `query mine` identify each task by its bare `task_id` (UUID) only — **not** the friendly `id`, the repo, or the GitHub `#NNN`. To show the friendly `rpl-…` id (whose prefix tells you which repo a task belongs to) or the issue number, cross-reference `rl task list --workspace <id>` — its entries carry `id`, `repo_id`, and `remote.remote_id` — and match by title.
- `query mine` resolves its assignee as `--assignee` > `git config user.name` > `$REPO_LINK_USER` > `$USER` — it does **not** use the GitHub login. But `task claim` assigns your *cached GitHub login* (`rl gh auth`), which usually differs from your committer name. So a task you've claimed can be missing from a bare `query mine`; pass `--assignee <github-login>` to be sure.

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
rl task start <task-id>             # local-only lifecycle nudge: Open|Blocked → InProgress
rl task claim <task-id>             # public commitment: assign me + start + push to GitHub
```

`start` and `claim` differ in **what other people see**, not in the lifecycle transition itself — both move the task to `InProgress`. Pick by audience:

- **`task start`** — flip a task to `InProgress` for your own queries (`query ready` / `query mine`) without touching `assignees` or the remote issue. Use it for purely-local tasks, for tasks you're not announcing yet, or when you want to start work without changing who owns the issue. Idempotent and offline-safe.
- **`task claim`** — announce externally that you've picked up the task: appends the authenticated GitHub user to `assignees` (merge, not replace), runs the same `Open|Blocked → InProgress` transition, and best-effort `sync push`-es the change so teammates, the issue list, and project boards reflect it. Use it the moment your work becomes something others should coordinate around. Pass `--no-sync` to skip the push for local-only / staged tasks. Requires the cached GitHub login — run `rl gh auth` once to populate it.

Rule of thumb: if nobody else needs to know you've started, `start` is enough. If anybody else might pick up the same task, run `claim` so they don't.

### Revising a task's content

To change a task's title, body, priority, or assignees in place — without losing its identity or audit trail — use `rl task edit`:

```bash
rl task edit <task-id> --title "new title"
rl task edit <task-id> --body "new body" --priority p1
rl task edit <task-id> --assignee alice --assignee bob   # replace-set; omit --assignee entirely to keep the current list
```

At least one of `--title` / `--body` / `--priority` / `--assignee` must be supplied — `rl task edit <task-id>` with no flags is rejected (use `rl task show <task-id>` to inspect current values without changing anything). Clearing all assignees is not expressible via `edit`; that's deliberate.

`task edit` bumps a new row in the task's snapshot history (`source = local_edit`); a subsequent `rl task rollback <task-id> --to-version <N>` can restore any earlier version. **Do not** use `archive + create` to revise a task — that produces a different task (new UUID, lost history) and breaks any shared references to the old ID.

### Before you stop: sync your work

When a session is wrapping up, never leave local work unpushed. The flow is:

```bash
rl query unsynced --workspace <id>           # see what's pending
rl sync promote   <task-id>                  # Draft/Staged → create remote issue
rl sync push      <task-id>                  # DirtyLocal → push local edits to remote
rl task complete  <task-id>                  # mark done locally (then push if needed)
```

Then re-run `rl query unsynced --workspace <id>` and confirm it returns an empty list. If it doesn't, the session isn't done — either push the remaining tasks or note them as deliberate carry-over for the next session.

### Referencing tasks in pull requests

When opening a pull request, the PR title, body, commit messages, and branch name may reference issues **only** by their GitHub URL or `#NNN` number.

Do **not** paste an `rl` task UUID, the task's short prefix / friendly ID (`rpl-ev6`), a `task-id` of any form, a workspace UUID, or any other local-only identifier into a PR description, commit message, or branch name. Those identifiers are invisible to anyone reading the PR on GitHub and rot the moment the local DB is reset or the workspace is recreated.

If a local-only task needs to be referenced from a PR, the correct flow is: `rl sync promote <task-id>` first (which creates the remote GitHub issue), then reference *that* issue (`#NNN`) in the PR. Never the reverse.

The same rule applies to commit trailers, changelog entries, and any other artifact that lives in the git history.

### Useful filters and views

```bash
rl query blocked      --workspace <id>   # open tasks blocked by an open dependency (BlockedBy)
rl query stale        --workspace <id>   # open tasks untouched for a while
rl query contributors --workspace <id>   # who has touched which tasks
rl task snapshots <task-id>              # full version history of one task
```
