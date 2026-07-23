`rl` (repo-link) is a local-first workspace and task manager that syncs to GitHub Issues.

## When to use `rl`

Use `rl` only when the request concerns tracked work:

- choosing the next task;
- inspecting or changing an `rl` task;
- synchronizing a task with GitHub Issues;
- resolving workspace or repository scope.

Do not invoke `rl` merely because the checkout is bound to a workspace.

All commands emit JSON on stdout. Use `jq` to extract or reshape fields, and present results to a human as a markdown table rather than raw JSON. Run `rl <subcommand> --help` (or `rl <subcommand> <verb> --help`) for the authoritative flag reference.

### Workspace context

When an `rl` workflow needs workspace context, run:

```bash
rl here
```

It returns every workspace this checkout belongs to, the current repo binding, filing repo, and sibling repos. Use the returned `workspace.id` as `--workspace <id>`. An empty `matches` array means the checkout is unbound.

### Choosing work

For an explicit “what should I work on?” request, ask `rl` what is actionable:

```bash
rl query ready --workspace <id>
rl query mine  --workspace <id>
```

`query ready` accounts for transitive blockers and local-only tasks that GitHub cannot show.

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
