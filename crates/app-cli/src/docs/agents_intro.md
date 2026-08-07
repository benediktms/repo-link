`rl` (repo-link) is a local-first workspace and task manager that syncs to GitHub Issues.

## When to use `rl`

Use `rl` only when the request concerns tracked work:

- choosing the next task;
- locating a task the user describes but cannot name;
- inspecting or changing an `rl` task;
- synchronizing a task with GitHub Issues;
- resolving workspace or repository scope.

Do not invoke `rl` merely because the checkout is bound to a workspace.

Data-producing commands emit JSON on stdout; help and diagnostics may be human-readable. Present results to a human as a markdown table rather than raw JSON. Run `rl <subcommand> --help` (or `rl <subcommand> <verb> --help`) for the authoritative flag reference.

### Workspace and repository context

When an `rl` workflow needs workspace or repository context, use:

```bash
rl here                    # resolve the current checkout
rl repo find <query>       # search bindings by name, alias, or URL
rl repo list               # list all active-workspace bindings; add --workspace to scope
```

`rl here` returns every workspace the checkout belongs to, its repo binding, filing repo, and sibling repos. Use the returned `workspace.id` as `--workspace <id>`. An empty `matches` array means the checkout is unbound. `rl repo list` works outside a bound checkout; its optional `--workspace <id>` flag scopes the global result to one workspace.

### Finding a specific task

When the request describes one particular task rather than asking what to do next, search — do not list and filter:

```bash
rl task search "<the user's own words>"
rl task search "PortError::Backend" --exact   # force substring, skip query classification
```

`task search` retrieves over task titles, bodies, and comments, and classifies the query itself (exact / identifier / natural), so a paraphrase finds the task even when it shares no keyword with the title. Each result carries the friendly `id`, the workspace name, and the excerpt that matched — quote that excerpt as evidence. Reach for this before any listing piped through a substring filter, and before searching GitHub.

The response reports `lexical_available` and `semantic_available`. Search degrades rather than fails, but recall on paraphrased queries drops sharply without the semantic lane. When it is unavailable, say so and offer the remedy:

```bash
rl task search-index status          # lane availability, chunk and vector counts
rl task search-index prepare-model   # one-time: fetch and verify the pinned embedding model
rl task search-index rebuild         # re-chunk and re-embed; the index is disposable
```

### Choosing work

For an explicit “what should I work on?” request, ask `rl` what is actionable:

```bash
rl query ready                     # ready frontier for this repo's workspaces (all workspaces if unbound)
rl query ready --local             # only this repo's own ready tasks
rl query ready --workspace <id>    # a single workspace
rl query mine  --workspace <id>
```

`query ready` accounts for transitive blockers and local-only tasks that GitHub cannot show, and returns the ready tasks as a nested parent→child tree ordered by priority.

### Working with a tracked task

Inspect the task before changing it:

```bash
rl task show <task-id>
```

Before changing a remote-backed task, run `rl query drift --workspace <id>`. If that task has drifted, reconcile only that task before editing it.

Limit `claim`, `edit`, `complete`, `promote`, `push`, and `pull` operations to tasks involved in the current request. Do not modify or synchronize unrelated tasks returned by workspace-wide queries.

If the request changed a remote-backed task, push that task before finishing or report why it remains unsynced. Do not promote a local-only task without user intent.

### Referencing tasks in remote content

Use a GitHub issue URL or `#NNN` in pull requests, issues, commit messages, branch names, changelogs, and code comments. Never use an `rl` task ID, friendly ID, task UUID, or workspace UUID in remotely hosted content.
