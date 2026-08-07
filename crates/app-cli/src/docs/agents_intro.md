`rl` (repo-link) is a local-first workspace and task manager that syncs to GitHub Issues.

## When to use `rl`

Use `rl` only when the request concerns tracked work: choosing the next task, locating a task the user describes but cannot name, inspecting or changing a task, synchronizing one with GitHub Issues, or resolving workspace and repository scope. Do not invoke `rl` merely because the checkout is bound to a workspace.

Data-producing commands emit JSON on stdout; help and diagnostics may be human-readable. Present results to a human as a markdown table rather than raw JSON. Run `rl <subcommand> --help` (or `rl <subcommand> <verb> --help`) for the authoritative flag reference — what follows is only what `--help` cannot tell you.

### Workspace and repository context

```bash
rl here                    # resolve the current checkout
rl repo find <query>       # search bindings by name, alias, or URL
rl repo list               # list all active-workspace bindings; add --workspace to scope
```

`rl here` returns every workspace the checkout belongs to, its repo binding, filing repo, and sibling repos. Use the returned `workspace.id` as `--workspace <id>`; an empty `matches` array means the checkout is unbound. Most commands infer the workspace from the cwd, so pass `--workspace` only to narrow the result or when `rl` reports an ambiguous checkout. `rl repo list` also works outside a bound checkout.

### Finding a specific task

When the request describes one particular task rather than asking what to do next, search — do not list and filter:

```bash
rl task search "<the user's own words>"
```

Retrieval covers titles, bodies, and comments, and classifies the query itself, so a paraphrase finds the task even when it shares no keyword with the title. Each result carries the friendly `id` and the excerpt that matched; quote that excerpt as evidence. Reach for this before any listing piped through a filter, and before searching GitHub.

The response reports `lexical_available` and `semantic_available` — check them. Search degrades rather than fails, so without the semantic lane a thin result set is not evidence that no such task exists. Say the lane is down and offer `rl task search-index prepare-model`.

### Choosing work

For an explicit “what should I work on?” request, ask `rl` what is actionable:

```bash
rl query ready                     # ready frontier for this repo's workspaces (all workspaces if unbound)
rl query ready --local             # only this repo's own ready tasks
```

`query ready` accounts for transitive blockers and local-only tasks that GitHub cannot show, and returns the ready tasks as a nested parent→child tree ordered by priority — recurse it rather than reading it as a flat list. Its entries carry the bare `task_id` UUID; titles are not unique, so do not try to recover a friendly `id` by matching them against another listing.

### Working with a tracked task

Inspect the task before changing it:

```bash
rl task show <task-id>
```

Before changing a remote-backed task, run `rl query drift`. If that task has drifted, reconcile only that task before editing it.

Limit `claim`, `edit`, `complete`, `promote`, `push`, and `pull` operations to tasks involved in the current request. Do not modify or synchronize unrelated tasks returned by workspace-wide queries. The three `sync` verbs take `--task <id>`, not a positional argument.

If the request changed a remote-backed task, push that task before finishing or report why it remains unsynced. Do not promote a local-only task without user intent.

### Referencing tasks in remote content

Use a GitHub issue URL or `#NNN` in pull requests, issues, commit messages, branch names, changelogs, and code comments. Never use an `rl` task ID, friendly ID, task UUID, or workspace UUID in remotely hosted content.
