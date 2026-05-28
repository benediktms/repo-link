# RFC 0001 — Workspace ↔ GitHub: project sync and authoritative state ownership

Status: **Accepted (design)** — informs follow-up implementation tickets.
Spike issue: [#28](https://github.com/benediktms/repo-link/issues/28).
Subsumes: former #15 (octocrab evaluation).
Unblocks: [#39](https://github.com/benediktms/repo-link/issues/39) (drift/pull bug).

## 1. Context

Today `repo-link` reconciles local task state against GitHub via the REST API alone. That's a half-story: a task's **status on a Projects v2 board** is invisible to REST. Without GraphQL, we cannot:

- Read the project status when fetching an issue (so drift detection lies — `rl sync pull` says "clean" even though the project column changed).
- Write a project status when the user runs `rl task start` / `complete` / `block` (so lifecycle changes only flip open/closed, never move the card).

This RFC answers four coupled questions in one pass:

1. **Local projection.** How do projects fit into the existing `Workspace ⊃ RepoBinding ⊃ Task` model?
2. **Authoritative state ownership.** Who owns a synced task's lifecycle — the local store or GitHub?
3. **GraphQL adapter.** How does `infra-github` gain GraphQL — `octocrab` or hand-rolled on `reqwest`?
4. **Polling cadence.** Without webhooks, how do we keep the local cache fresh without crushing the rate limit?

It also lays out the hexagonal-architecture-aligned split for implementation. Webhooks and `rl sync pull --all` are explicitly out of scope (see §9 follow-ups).

## 2. Findings from schema and live data

Full introspection lives in [Appendix A](#appendix-a-graphql-shapes-we-care-about); the load-bearing facts:

1. **Status is a custom field, not a project property.** `ProjectV2` has no `status` — instead `fields { ProjectV2SingleSelectField { options[] } }`. Both the field ID and the option ID are needed to set status. The field's *name* defaults to "Status" but can be renamed.

2. **Status options are user-defined, per project.** A live probe of `viewer.projectsV2` returned three different option sets across three projects:
   - `repo-link` project: `Backlog → Ready → In progress → In review → Done` (5 options).
   - Two untitled projects: `Todo → In Progress → Done` (the GitHub default).

3. **Option sequence is array-order, not an explicit field.** `ProjectV2SingleSelectFieldOption` has `{id, name, color, description, descriptionHTML, nameHTML}` — no `position`. The order matches the UI's field-settings panel because GraphQL returns the array in that order. Stable in practice; we capture it as an `ordinal` column at fetch time.

4. **Issue ↔ Project is many-to-many at the API.** `Issue.projectItems` lists every project; `ProjectV2.items.content` is the inverse. In practice, the typical case is one project per repo, one repo per project.

5. **Projects are user/org-scoped, not repo-scoped.** A project lives under `ProjectV2Owner` (`User|Organization`). It has a `repositories` connection but isn't owned by a repo — rules out modeling projects under `RepoBinding`.

6. **Mutations require IDs from a prior query.** `updateProjectV2ItemFieldValue(projectId, itemId, fieldId, value: { singleSelectOptionId })` is the workhorse. `addProjectV2ItemById(projectId, contentId)` adds an issue to a project.

7. **Workflows are listed but opaque.** `ProjectV2Workflow` exposes `{id, name, number, enabled}` only — *no trigger conditions, no actions, no target columns*. GitHub's own automations ("Item added to project → Backlog") cannot be introspected. Practical consequence: after `addProjectV2ItemById`, we always explicitly set the status field rather than trusting the workflow default. Race-free because both mutations are sequential from our side.

8. **`ProjectV2.items` supports server-side delta filtering.** The `query:` argument accepts GitHub's full issue-search syntax. Verified live:
   ```graphql
   items(first: 100, query: "is:open updated:>2026-05-25")
   # returned 8 items of 13 open
   ```
   Combined with `content { ... on Issue { title, body, state } }` and `fieldValueByName(name: "Status")`, **one query per project per poll returns both axes**.

9. **`ProjectV2ItemOrderField` has only `POSITION`** — no `UPDATED_AT` sort. The `query:` filter is the delta lever; ordering doesn't help.

10. **REST is repo-scoped, GraphQL is project-scoped.** `/repos/o/r/issues?since=` misses **draft issues** (project-only, no repo) and **issues in unbound repos within the same project**. For project-aware polling, REST delta is structurally insufficient.

11. **`infra-github` was already designed for this.** The crate's `lib.rs` docstring anticipates a sibling `graphql` module composed with the REST one under `GithubTaskProvider`. The port boundary stays small.

## 3. Decisions

### D1 — Local projection: **Project 1 → N Workspaces (workspace.project_id nullable)**

```text
Project ─→ N × Workspace ─→ N × RepoBinding ─→ N × Task
   │                                              │
   └────────── owns status schema ────────────────┘
                  (inherited by all child workspaces)
                                                  │
                                  task.project_item_id (lazy, set on first push)
```

- A new `Project` aggregate holds `{id, provider, owner_login, number, title, status_field_id, archived}` where `id` is the GitHub node ID directly (`PVT_…`) — no separate local UUID. Its options live in a sibling collection (see §6 schema).
- `Workspace` gains an optional `project_id` (nullable FK). Workspaces without a project remain valid — they're the local-only path. The migration is therefore additive: existing rows get `project_id = NULL` and behave exactly as today.
- `Task` gains optional `project_item_id` (`PVTI_…`), populated lazily on first push.
- Project membership for any given task is derived: `task → workspace → project`. There is no `RepoBinding.default_project_id` and no join table.

**Why this shape (vs. earlier alternatives):**

The earlier draft of this RFC put Project as a *child* of Workspace (Workspace → N Projects via a join). That was upside-down. A GitHub Projects v2 board is the **long-lived container** with the status schema — it persists across many deliverables. A workspace is the unit of *deliverable*: a feature, a release, a slice of work that ships and then gets archived. So workspaces belong inside projects, not the other way around.

Concretely:

- **Status mapping ownership is unambiguous.** The mapping lives on the Project; every workspace under that project inherits it. Two workspaces under the same project cannot drift on what "In progress" means. Under the previous shape, the mapping was per-workspace, which made no sense given the project owns the option list.
- **GitHub alignment.** A Project IS the top-level container in GitHub's mental model (owner-scoped, repo-spanning). Modeling workspaces underneath matches the API.
- **Workspaces get a parent they need.** Today's flat workspaces have nowhere to belong; archiving one loses the "what was this part of?" context. The inversion gives that context a name.
- **Cheaper migration.** One nullable FK on the existing `workspaces` table, vs. the join table + override column the previous shape needed.

**What stays optional:**

A workspace may have `project_id IS NULL`. That's the local-only path: workspaces that don't correspond to any GitHub project. Adding a project later is a single `rl workspace set-project` call — purely additive, no data movement.

**What goes away (vs. previous draft):**

`RepoBinding.default_project_id` is removed. The "which project does this task sync to?" walk is `task → workspace → project`. One FK chain, no override column.

**Creation defaults and sync target (project workspaces):**

When a workspace has a `project_id`, the project is *always* the primary sync target — regardless of whether the task also has a repo. The repo is an *additional* attachment, not an alternative. Three creation paths fall out cleanly because `task.repo_id` is already nullable in the existing schema:

| `rl task create` invocation | Local representation | What `sync promote` does |
|---|---|---|
| In a project workspace, no `--repo` | **Orphan task**: `repo_id = NULL`. Pure project task. | `addProjectV2DraftIssue(project_id, title, body)` → draft item in the project. Cache `project_item_id`. No REST issue. |
| In a project workspace, with `--repo <binding>` | Repo-anchored task: `repo_id = <binding>`, also project-bound via workspace. | REST `POST /repos/o/r/issues` → real issue. Then `addProjectV2ItemById(project_id, issue.node_id)` → add to project. Then `updateProjectV2ItemFieldValue` → set status. |
| In a projectless workspace, with `--repo <binding>` | Today's behavior. | Today's REST-only behavior. |

**Orphan → repo-anchored conversion.** A task that started as an orphan-draft can later be attached to a repo via `rl task edit <id> --repo <binding>`. On the next sync, the daemon issues `convertProjectV2DraftIssueItemToIssue(item_id, repo_node_id)` — the same `project_item_id` is preserved, the content type changes from `ProjectV2DraftIssue` to `Issue`. This is the "I triaged in the project; now I know which repo it belongs to" path the user explicitly called out.

**Configurability deferred.** A future user may want "project workspace but file as real issue by default in some specific repo." That's a `Workspace.creation_default_repo_id` column we can add cheaply later. Not in v1.

**No cross-repo transfer in v1.** If a task already has a `remote_id` (real issue) and `rl task edit --repo` points to a *different* binding, we reject. GitHub's `transferIssue` mutation exists but introduces enough complexity (cross-org permissions, label remapping) to warrant a separate ticket.

**Status mapping** (per project, stored alongside option rows):

| local `TaskStatus` | auto-derived default option |
|---|---|
| `Open` | first option matching `/^(backlog\|todo\|open\|new)$/i`, else first option |
| `InProgress` | first option matching `/^(in.progress\|doing\|wip)$/i` |
| `Blocked` | first option matching `/^(blocked\|on.hold\|waiting)$/i`, else fall back to `Open` |
| `Done` | first option matching `/^(done\|complete\|closed\|shipped)$/i`, else last option |
| `Archived` | no project mutation (REST `close as not_planned` handles this) |

All patterns are anchored (`^...$`) so they match the option's full name exactly — substring matching would let "Not done" trip the `Done` rule. Auto-derivation runs once at `link` time; the resulting map is stored and editable via `rl project map`.

**Fallback semantics — app level, not DB.** Most projects don't have a `Blocked`-like option. When no option matches the Blocked regex, no row in `project_status_mappings` claims `status = 'blocked'`. The application layer reuses the `Open` mapping when a task transitions to Blocked. This is app-level fallback only: rather than persisting a second `(blocked → same option)` row, we leave `blocked` unmapped and resolve it to the `Open` option at lookup time. (The schema *can* represent `Open` and `Blocked` both pointing at one option — many statuses → one option is a first-class case — so making this fallback explicit later is a non-breaking change.)

**Project identity is the GitHub node ID.** `ProjectId` is a newtype wrapping a String (validated as a GitHub `PVT_…` node ID), defined in `domain-core` — *not* a `define_id!`-style UUID. Project records are 100% mirrors of the GitHub entity; we don't generate a local identity for them. See §6 for the type sketch.

**Field selection** (briefly — full rationale in [Appendix A](#appendix-a-graphql-shapes-we-care-about)): the live `repo-link` project has multiple single-select fields (`Status`, `Priority`, `Size`). The auto-mapping picks the one *literally named* "Status"; if no such field exists, it falls back to the first single-select field. The chosen field's `id` is stored as the project's `status_field_id`.

### D2 — Authoritative state ownership: **mirror + outbox**

This is the conceptual cleanup the spike circled to. A *synced* task is a mirror of an external object; we don't own its lifecycle.

**Principles:**
- A task's lifecycle is owned by **whoever the source of truth is**. For `LocalOnly` tasks, that's us. For tasks with a remote ID, it's GitHub.
- The local SQLite row for a mirror task is a **write-through cache** of GitHub's last-known state.
- Outbound mutations go through an **outbox**: any `rl task start / complete / block / edit` on a mirror task enqueues a pending mutation and (when online) drains it immediately.
- The "mirror vs local" distinction is **derived**, not encoded as a new column or status. `task.sync_state != LocalOnly` ⇒ mirror. Draft-backed mirrors additionally have `remote_id IS NULL AND project_item_id IS NOT NULL` (drafts have no REST issue number); issue-backed mirrors have `remote_id IS NOT NULL` regardless of project membership.

**This is the rebranding of existing dirty/clean machinery.** `DirtyLocal` today already means "local moved, remote hasn't caught up." Calling that an outbox entry makes the model honest about what it is — and closes one gap: a task created offline, edited offline, then synced should drain N outbox entries in order, not flatten them into one "current state pushed."

**`TaskStatus` does not change.** Adding `ExternalSync` as a status value would collapse two orthogonal axes (lifecycle vs. ownership). They stay separate: `status` is lifecycle, `sync_state` is ownership.

**Reads stay cache-first.** `rl task show` returns the cache. Drift detection (the polling loop, §D4) invalidates entries; the next read pulls fresh.

**Workflow opacity caveat.** When we `addProjectV2ItemById`, GitHub's own "Item added to project" workflow may set status to e.g. "Backlog." Our follow-up `set_status` overwrites it. Race-free because both calls are sequential from our side — but we always issue the explicit `set_status` rather than relying on the workflow default (which is unintrospectable, see §2.7).

**Drafts are mirrors too.** A draft project item is a first-class mirror task: lifecycle is owned by the project (status field), title/body live on the draft, no REST open/closed exists. The outbox handles draft mutations (`CreateDraft`, `UpdateDraft`, `ConvertDraft`) identically to issue mutations — same enqueue/drain/retry machinery. Reads of an orphan-draft task come from the cache; writes go through the outbox.

### D3 — GraphQL adapter: **adopt `octocrab` + `graphql_client`**

`infra-github` is REST-only today (~430 LoC). Two paths considered:

- **Option A (chosen): `octocrab` for REST + `octocrab.graphql()` for v2.**
- **Option B (rejected): hand-rolled REST + minimal reqwest-based GraphQL POST.**

**Why A:**
- The port boundary (`RemoteTaskProvider`, soon `RemoteProjectProvider`) keeps the choice reversible. Vendor lock-in stays inside `crates/infra-github/`.
- We're already missing retry/backoff on 429/secondary rate limits. Hand-rolling that for both REST and GraphQL is duplicative; `octocrab`'s `retry` feature gives it for free via tower.
- The `tracing` feature integrates with the daemon's subscriber from PR #38 — every HTTP call becomes a structured span.
- ETag/`If-Modified-Since` is built in via `OctocrabBuilder::cache(InMemoryCache::default())`. Conditional GETs don't decrement the rate-limit counter.
- Pagination ships as `Page<T>` + `octocrab.get_page()` — replaces a Link-header parser we don't have.

**Typed vs raw GraphQL.** `octocrab.graphql(query)` accepts any string and returns `GraphqlResponse<T>` where `T` is your custom serde struct. The idiomatic add-on is the `graphql_client` crate: `#[derive(GraphQLQuery)]` against checked-in `*.graphql` query files + an introspection dump at `crates/infra-github/schemas/github.graphql`. Compile-time validation, refreshable via `gh api graphql -F query=@introspection.graphql > crates/infra-github/schemas/github.graphql`. **We use `graphql_client` for v2 work**; raw strings only as an escape hatch.

**One honest limitation.** `octocrab::projects` is the *legacy v1* REST projects API (which GitHub is sunsetting). There is no typed v2 surface in octocrab — there can't be in any client, because GitHub made v2 GraphQL-only. All v2 work goes through `octocrab.graphql()`. See [Appendix B](#appendix-b-octocrab-capability-map) for the full feature table.

### D4 — Polling cadence: **GraphQL `items(query:)` per project, REST fallback for binding-only**

The daemon polls each linked project every 30–60 seconds with a single GraphQL query:

```graphql
project.items(first: 100, query: "is:open updated:>$last_poll") {
  nodes {
    id
    updatedAt
    fieldValueByName(name: "Status") { ... on ProjectV2ItemFieldSingleSelectValue { optionId name } }
    content { ... on Issue { number title body state assignees(first:10) { nodes { login } } } }
  }
}
```

Properties:
- Server-side filter on both axes (status freshness + open-only). Payload scales with change rate, not task count.
- Returns issue fields AND project status in one round-trip.
- `is:open` excludes Done/Archived churn; reopening a closed task is a manual `rl sync pull <id>` escape.
- Pagination kicks in only if >100 items changed in one tick — not realistic in steady state.

**Secondary path: REST for tasks in projectless workspaces** (`workspace.project_id IS NULL`). One `GET /repos/{owner}/{repo}/issues?state=all&since=$last_poll&per_page=100` per binding in those workspaces. Scales the same way — by change rate.

**Edge cases:**
- **Reopen-after-close** on a Done task: missed by the `is:open` filter. Acceptable — surfaced by a "last synced N days ago" hint on `rl task show <archived>`; force refresh via `rl sync pull <id>`.
- **Daemon offline window.** On reconnect, run one wide-window catch-up poll (`updated:>$last_known_tick - 5m`) before resuming the normal cadence. Bounded but complete.
- **Drift surfacing (closes #39).** Cache reconcile diffs against the cached `project_status_option_id`. A non-match emits a drift row visible to `rl query drift`.

Webhooks are explicitly deferred — see §9.

## 4. Hexagonal split — crate map

The repo is already cleanly layered (domain → ports ← infrastructure / application → ports). The new work fits the existing seams:

```text
                        ┌──────────────┐
                        │   domain-*   │  (pure types, no I/O)
                        └──────┬───────┘
                               │
                        ┌──────▼───────┐
                        │    ports     │  (trait surfaces)
                        └──┬───────┬───┘
              ┌────────────┘       └───────────┐
       ┌──────▼──────┐                  ┌──────▼──────┐
       │ application │                  │   infra-*   │
       │     -*      │ ────────────────▶│  (adapters) │
       └─────────────┘   composed by    └─────────────┘
                          app-cli / app-daemon
```

**Existing crates touched:**

| Crate | Change |
|---|---|
| `domain-core` | + `ProjectId` (newtype around `String`, validates as a `PVT_…` node ID — not a `define_id!` UUID). + `OutboxEntryId` (UUID via `define_id!`, pattern-aligns with existing IDs). |
| `domain-task` | + `project_item_id: Option<String>` field on `Task`. + `node_id: Option<String>` on `RemoteRef` (REST gives us the number; GraphQL gives us the node ID; we keep both). No status enum changes. |
| `domain-workspace` | + `project_id: Option<ProjectId>` field on `Workspace`. |
| `domain-sync` | + `OutboxEntry`, `OutboxMutation` (sum type over the supported mutations), `OutboxStatus` (pending/inflight/succeeded/failed). |
| `ports` | + `ProjectRepository`, + `RemoteProjectProvider`, + `OutboxRepository`, + `RemoteTaskProvider::list_changed_since` (REST delta). |
| `infra-sqlite` | + migrations for `projects`, `project_status_options`, `outbox_entries`, `tasks.project_item_id`, `tasks.remote_node_id`, `workspaces.project_id`. + repository impls. |
| `infra-github` | + `graphql` submodule implementing `RemoteProjectProvider`. Swap REST internals to `octocrab`. Capture `issue.node_id` from REST responses into `RemoteRef.node_id`. Rename `GithubTaskProvider` → `GithubAdapter` (no longer task-only). |
| `application-sync` | + outbox drainer task. + project poller task (calls `RemoteProjectProvider::poll_project_items`, reconciles cache). |
| `application-task` | Lifecycle verbs enqueue outbox entries when the task is a mirror; no behavior change for `LocalOnly` tasks. |
| `application-query` | + `rl query drift` includes `project_status` axis. |
| `app-cli` | + `rl project link/show/unlink/map`. + `rl workspace set-project <workspace> <project-spec>`. + `--project <project-spec>` flag on `rl workspace create`. + `rl sync pull --all` (separate follow-up ticket but called out here). |
| `app-daemon` | + restructure `Daemon::run` from one ticker to two concurrent background tasks (poller + outbox drainer) with shared cancellation. Cadences hardcoded as constants in v1; see Stage 7. |
| `testing-fixtures` | + in-memory `ProjectRepository`, `RemoteProjectProvider`, `OutboxRepository`. Follow the existing `Mutex<HashMap<Id, T>>` pattern from `InMemoryWorkspaceRepository`. |

**New crates added:**

| Crate | Owns |
|---|---|
| `domain-project` | `Project`, `StatusOption`, `StatusMapping`. Pure types + invariants (e.g. "mapping must reference an option owned by the project"). |
| `application-project` | `ProjectService` orchestrating the `RemoteProjectProvider` + `ProjectRepository` for link/unlink/map. |

Total: 11 crates touched, 2 new. No crate gains an unexpected dependency direction — domain stays at the bottom, infra and application stay at the top, ports stays in the middle.

## 5. Staged implementation plan

Eight stages, each independently mergeable and verifiable. Stages within a "lane" (e.g. domain + ports) can ship as one PR if scope stays small.

### Stage 1 — Infrastructure foundation (no functional change)
- 1a. Add `octocrab` to workspace deps. Swap REST internals in `crates/infra-github/src/rest.rs` to `octocrab::issues`. Existing wiremock tests stay structurally the same, but **fixtures expand significantly**: today's `issue_payload()` helper supplies 7 fields; octocrab's typed `Issue` model demands ~30 (`id`, `node_id`, `url`, `user`, `created_at`, `repository_url`, etc.). Plan for a deliberate fixture rewrite rather than a one-line addition.
- 1b. Add `graphql_client` to workspace deps. Check in `crates/infra-github/schemas/github.graphql` (introspection dump). No code uses it yet — sets up the toolchain.

PR shape: one PR (mechanical swap + tooling). Risk: low — port surface unchanged.

### Stage 2 — Domain + ports (additive, no behavior)
- 2a. New `domain-project` crate.
- 2b. Extend `domain-sync` with outbox types.
- 2c. Extend `ports` with `ProjectRepository`, `RemoteProjectProvider`, `OutboxRepository`, and `RemoteTaskProvider::list_changed_since`.
- 2d. Add `project_item_id` to `Task` (domain-task) + `project_id` to `Workspace` (domain-workspace).
- 2e. Add `ProjectId` to `domain-core` as a newtype wrapping `String` (validates non-empty + `PVT_` prefix). Add `OutboxEntryId` via the existing `define_id!` macro (it's purely local, no remote analogue).
- 2f. Extend `RemoteRef` in `domain-task` with `node_id: Option<String>`. Existing constructors default it to `None` — only GitHub paths will populate it. Storage adds `tasks.remote_node_id TEXT` (see §6).

PR shape: one PR. Risk: low — purely additive.

### Stage 3 — Storage adapters
- 3a. `infra-sqlite` migrations for the four schema changes (see §6).
- 3b. Implement `ProjectRepository`, `OutboxRepository` against SQLite.
- 3c. In-memory variants in `testing-fixtures`.

PR shape: one PR. Risk: low — `infra-sqlite` migrations are append-only and rehearsable.

### Stage 4 — Application service + CLI for project management (local-only)
- 4a. `application-project::ProjectService` with `link/unlink/get/map_status` accepting hand-entered project IDs.
- 4b. `rl project link <workspace> --node-id <PVT_…>` etc. — works without network access.

PR shape: one PR. Risk: low — exercise the new code paths through the CLI before any GitHub I/O.

### Stage 5 — GraphQL adapter
- 5a. Add `graphql` submodule to `infra-github`. Implement `RemoteProjectProvider` against `octocrab.graphql()` — including the draft path (`create_draft_issue`, `update_draft_issue`, `convert_draft_to_issue`) and the issue-attach path (`add_item`, `set_status`, `poll_project_items`).
- 5b. Rewire `rl project link` to fetch the project schema from GitHub (rather than accepting hand-entered IDs).
- 5c. wiremock-based tests for the GraphQL surface (mock the single `POST /graphql` endpoint). One test per mutation/query shape; one test for the draft → issue conversion path.

PR shape: one PR. Risk: medium — first GraphQL surface in the codebase. Live smoke test against this account's project #3 before merge.

### Stage 6 — Outbox refactor + lifecycle wiring
- 6a. `application-sync` gains the outbox drainer.
- 6b. `OutboxMutation` enum covers `UpdateRemote` (REST), `CreateDraft`, `UpdateDraft`, `ConvertDraft`, `AddItem`, `SetProjectStatus`.
- 6c. `application-task` lifecycle verbs (`start/complete/block/edit`) enqueue the appropriate variant when the task is a mirror, instead of direct-mutating synchronously. `task edit --repo` on an orphan-draft enqueues `ConvertDraft`.
- 6d. Drainer runs the existing REST mutations for `UpdateRemote`-class entries (no behavior change for projectless workspaces).

PR shape: one PR. Risk: medium-high — touches every lifecycle verb. Reviewer focus: invariant preservation around `sync_state` transitions.

### Stage 7 — Polling loop
- 7a. `application-sync` gains the project poller.
- 7b. **Restructure `Daemon::run` from one ticker to two concurrent background tasks.** Today the daemon has a single tick loop; Stage 7 splits it into two `tokio::spawn`'d tasks coordinated by a shared cancellation token. Wrap with `tokio::select!` over their `JoinHandle`s in the run loop so a panic in either still trips shutdown.
- 7c. **Cadences are hardcoded constants in v1**, with `// TODO(config): expose via infra-config once a user actually asks` comments at the call sites:
  - `PROJECT_POLLER_INTERVAL: Duration = 30..60s` for the GraphQL `project.items(query:)` polling path.
  - `OUTBOX_DRAINER_PERIODIC_SWEEP: Duration = 5s` as a safety net; the drainer's primary trigger is just-in-time — `rl task start/edit/complete/...` enqueues then signals the drainer immediately via a `tokio::sync::Notify`. The sweep only catches edges (e.g. enqueues that happened just before the drainer parked).
- 7d. `infra-config` gains no fields in this stage. Config knobs are deferred until someone actually needs them — see §9 follow-ups.

PR shape: one PR. Risk: medium — first real background work the daemon does beyond heartbeats.

### Stage 8 — Project status reads/writes + drift surfacing (closes #39)
- 8a. Outbox supports `SetProjectStatus` mutations; `task start/complete/block` enqueue them when the task has a `project_item_id`.
- 8b. `application-query::DriftRow` includes a `project_status` field.
- 8c. `rl task show` renders the cached project status.
- 8d. `rl query drift` shows mismatches.

PR shape: one PR. Closes #39. Risk: low if 6 and 7 are solid — Stage 8 is the consumer.

**Why this order:**
- Stages 1–3 are scaffolding — adapters and types without semantics. Cheap to review.
- Stage 4 ships a usable surface (`rl project link`) before any network code. Lets us exercise the model end-to-end with hand-entered IDs as a sanity check.
- Stage 5 puts real bytes on the wire but only for project metadata (read-only at this point).
- Stage 6 is the load-bearing refactor; isolated to its own PR so reviewers can focus on outbox invariants.
- Stages 7–8 layer on top once 6 is stable.

**What can ship without the others:** Stage 1 ships in isolation (octocrab REST swap). Stage 6 (outbox refactor) is technically separable from the project work — if we wanted to ship outbox first as pure cleanup, we could, but the joint reviewer narrative is stronger.

## 6. Port and schema sketches

### Port additions (`crates/ports/src/lib.rs`)

```rust
#[async_trait]
pub trait RemoteProjectProvider: Send + Sync {
    /// Resolve `owner/number` → project schema. Called by `rl project link`.
    async fn fetch_project(&self, owner: &str, number: u64) -> PortResult<RemoteProjectSnapshot>;

    /// Add an existing issue to a project. Returns the new item's node ID.
    /// Idempotent (re-calling for the same content returns the existing item ID).
    /// Used when promoting a repo-anchored task — REST creates the issue first,
    /// then this call attaches it to the project.
    async fn add_item(&self, project_node_id: &str, issue_node_id: &str) -> PortResult<String>;

    /// Create a draft issue directly in the project. Returns the new item's
    /// node ID. Used when promoting an orphan task (no `repo_id`).
    async fn create_draft_issue(
        &self,
        project_node_id: &str,
        title: &str,
        body: &str,
    ) -> PortResult<String>;

    /// Update a draft issue's title/body. Drafts have no REST counterpart,
    /// so this is the only mutation path for an orphan task's content.
    async fn update_draft_issue(
        &self,
        item_node_id: &str,
        title: Option<&str>,
        body: Option<&str>,
    ) -> PortResult<()>;

    /// Convert a draft item to a real issue in `repo_node_id`. The item
    /// retains its node ID; only the `content` union shifts from
    /// `ProjectV2DraftIssue` to `Issue`. Used when an orphan task gets
    /// `--repo` attached via `rl task edit`.
    async fn convert_draft_to_issue(
        &self,
        item_node_id: &str,
        repo_node_id: &str,
    ) -> PortResult<String>;

    /// Set an item's single-select status field. Works on both draft items
    /// and issue-backed items.
    async fn set_status(
        &self,
        project_node_id: &str,
        item_node_id: &str,
        status_field_id: &str,
        option_id: &str,
    ) -> PortResult<()>;

    /// Poll a project for items changed since `since` matching `query`
    /// (e.g. "is:open"). Returns both issue-backed items AND drafts; the
    /// `RemoteProjectItem.issue_node_id` is `None` for drafts.
    async fn poll_project_items(
        &self,
        project_node_id: &str,
        since: Timestamp,
        query: &str,
    ) -> PortResult<Vec<RemoteProjectItem>>;
}

#[derive(Clone, Debug)]
pub struct RemoteProjectSnapshot {
    /// PVT_… — also the value stored as `projects.id` locally (no separate UUID).
    pub node_id: String,
    pub number: u64,
    pub title: String,
    pub owner_login: String,
    pub status_field_id: String,   // PVTSSF_…
    pub status_options: Vec<RemoteProjectStatusOption>,
}

#[derive(Clone, Debug)]
pub struct RemoteProjectStatusOption {
    pub option_id: String,         // 47fc9ee4
    pub name: String,
    pub ordinal: u32,              // array index from the API response
}

#[derive(Clone, Debug)]
pub struct RemoteProjectItem {
    pub item_node_id: String,
    pub issue_node_id: Option<String>,  // None for draft issues
    pub canonical_repo: Option<String>,
    pub number: Option<u64>,
    pub title: String,
    pub body: String,
    pub closed: bool,
    pub status_option_id: Option<String>,
    pub updated_at: Timestamp,
}

#[async_trait]
pub trait ProjectRepository: Send + Sync {
    async fn save(&self, project: &Project) -> PortResult<()>;
    async fn get(&self, id: ProjectId) -> PortResult<Project>;
    async fn list_by_workspace(&self, ws: WorkspaceId) -> PortResult<Vec<Project>>;
    async fn delete(&self, id: ProjectId) -> PortResult<()>;
}

#[async_trait]
pub trait OutboxRepository: Send + Sync {
    async fn enqueue(&self, entry: &OutboxEntry) -> PortResult<()>;
    async fn next_pending(&self) -> PortResult<Option<OutboxEntry>>;
    async fn mark_succeeded(&self, id: OutboxEntryId) -> PortResult<()>;
    async fn mark_failed(&self, id: OutboxEntryId, error: &str) -> PortResult<()>;
    async fn list_pending(&self, task_id: TaskId) -> PortResult<Vec<OutboxEntry>>;
}

// On the existing trait:
#[async_trait]
pub trait RemoteTaskProvider: Send + Sync {
    // … existing methods …
    /// Used by the REST polling fallback for binding-only tasks.
    async fn list_changed_since(
        &self,
        canonical_repo: &str,
        since: Timestamp,
    ) -> PortResult<Vec<RemoteTaskSnapshot>>;
}
```

Note: issue node IDs (`I_…`) come back from REST via octocrab's typed `Issue::node_id` *after* the Stage 1 swap; today's hand-rolled adapter (`crates/infra-github/src/rest.rs`) throws them away. Stage 1a captures the node ID into `RemoteRef.node_id` (see Stage 2f) so `add_item` has what it needs at Stage 5. No new REST endpoint or extra round-trip is involved.

### SQLite schema additions

```sql
-- Projects are workspace-independent: a project can be the parent of many
-- workspaces. The PK `id` IS the GitHub node ID (PVT_…) — projects are a
-- 100% mirror of the remote entity, so there's no separate local UUID.
CREATE TABLE projects (
  id                TEXT PRIMARY KEY,    -- PVT_… (the GitHub node ID itself)
  provider          TEXT NOT NULL CHECK (provider IN ('github')),
  owner_login       TEXT NOT NULL,
  number            INTEGER NOT NULL,
  title             TEXT NOT NULL,
  status_field_id   TEXT NOT NULL,       -- PVTSSF_…
  archived          INTEGER NOT NULL DEFAULT 0, -- mirrored from GitHub; cosmetic only — no cascade
  created_at        TEXT NOT NULL,
  updated_at        TEXT NOT NULL
);

CREATE TABLE project_status_options (
  project_id        TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  option_id         TEXT NOT NULL,
  name              TEXT NOT NULL,
  -- captured from the GraphQL response array index at fetch time. UI order.
  ordinal           INTEGER NOT NULL,
  PRIMARY KEY (project_id, option_id)
);

-- The local-status → project-option mapping. Its own table (not a column on
-- project_status_options) precisely because the relationship is **many
-- statuses → one option**: e.g. Open + Blocked both → "Backlog" on a board
-- with fewer columns than we have local statuses. Keying on
-- (project_id, status) enforces "one option per status per project" at the
-- DB — the same invariant `Project::new` checks in code — while leaving
-- option_id free to repeat. The composite FK keeps every mapping pointing at
-- an option the project owns and cascades mappings away when an option is
-- dropped during the wholesale option-set replace.
CREATE TABLE project_status_mappings (
  project_id        TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  status            TEXT NOT NULL CHECK (status IN ('open','in_progress','blocked','done')),
  option_id         TEXT NOT NULL,
  PRIMARY KEY (project_id, status),
  FOREIGN KEY (project_id, option_id)
    REFERENCES project_status_options(project_id, option_id) ON DELETE CASCADE
);

-- Workspaces gain an optional parent project. Existing rows migrate cleanly
-- with project_id = NULL (the local-only path).
ALTER TABLE workspaces ADD COLUMN project_id TEXT
  REFERENCES projects(id) ON DELETE SET NULL;

ALTER TABLE tasks ADD COLUMN project_item_id TEXT;  -- PVTI_…
-- Partial index: the polling loop looks up local tasks by item ID per
-- polled row. Excluding NULLs keeps the index small for projectless tasks.
CREATE INDEX idx_tasks_project_item_id ON tasks(project_item_id)
  WHERE project_item_id IS NOT NULL;

-- GitHub gives us two coexisting identities for the same issue: REST returns
-- a per-repo `number` (already stored as `remote_id`); GraphQL needs the
-- global `node_id` for project mutations. We persist both so we never have
-- to translate one to the other on the hot path.
ALTER TABLE tasks ADD COLUMN remote_node_id TEXT;  -- I_… (the GitHub issue node ID)

CREATE TABLE outbox_entries (
  id                TEXT PRIMARY KEY,
  task_id           TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  mutation_kind     TEXT NOT NULL,       -- 'update_remote' | 'set_project_status' | …
  payload_json      TEXT NOT NULL,       -- serialized OutboxMutation
  status            TEXT NOT NULL CHECK (status IN ('pending','inflight','succeeded','failed')),
  attempts          INTEGER NOT NULL DEFAULT 0,
  last_error        TEXT,
  enqueued_at       TEXT NOT NULL,
  updated_at        TEXT NOT NULL
);
CREATE INDEX idx_outbox_pending ON outbox_entries(status, enqueued_at) WHERE status = 'pending';
```

## 7. CLI surface

```bash
# Link a project from GitHub. Fetches schema + auto-derives status mapping.
# Projects are workspace-independent — multiple workspaces can attach to the
# same project afterwards.
rl project link <owner/number>
# Example: rl project link benediktms/3

# List known projects (across all workspaces).
rl project list

# Show a project, including child workspaces and the status mapping.
rl project show <project-spec>

# Override the auto-derived mapping.
rl project map <project-spec> --local in_progress --option-id 47fc9ee4

# Attach a workspace to a project (or detach with --none).
rl workspace set-project <workspace> <project-spec>
rl workspace set-project <workspace> --none

# Create a workspace already attached to a project.
rl workspace create <name> --project <project-spec>

# Unlink a project (local-only; doesn't touch GitHub). Workspaces attached to
# it get project_id reset to NULL.
rl project unlink <project-spec>
```

`<project-spec>` is `owner/number` or a project ID.

**`rl task create` changes (project workspaces only):**

```bash
# Workspace has a project: no --repo required.
# Creates an orphan task locally (repo_id = NULL).
# On sync, becomes a draft issue in the project.
rl task create --title "triage me"

# Workspace has a project AND user passes --repo:
# Creates a repo-anchored task. On sync, REST issue + project membership.
rl task create --title "fix the parser" --repo backend

# Orphan → repo-anchored conversion (triggers convertProjectV2DraftIssueItemToIssue
# on next sync). Cross-repo transfer of an already-real issue is rejected.
rl task edit <id> --repo backend
```

Other lifecycle verbs (`start/complete/block/edit` for non-`--repo` flags) gain no new behaviour. When a task's workspace has a `project_id`, the existing lifecycle transitions transparently enqueue the project-status mutation alongside the REST one.

## 8. Testing strategy

- **`infra-github` REST**: existing wiremock tests carry over post-octocrab swap (Stage 1).
- **`infra-github` GraphQL** (Stage 5): wiremock the single `POST /graphql` endpoint. Each test asserts the outgoing query shape and feeds back a JSON response.
- **`domain-project` invariants**: unit tests for the regex auto-mapping against the three live shapes from §2.2 plus a fully-custom set.
- **`application-project`** (Stage 4): in-memory `ProjectRepository` + `RemoteProjectProvider` fixtures.
- **Outbox semantics** (Stage 6): integration tests in `application-sync` covering enqueue → drain → success, enqueue → drain → fail → retry, and ordering preservation under multiple enqueues.
- **Polling** (Stage 7): test against a stubbed `RemoteProjectProvider::poll_project_items` returning canned `RemoteProjectItem` lists; assert local cache updates and drift rows.
- **Drift** (Stage 8): test the "REST says open, project says Done" and "REST says closed, project says In progress" cases.
- **Live smoke**: a manual run against this account's project #3 before each GraphQL-touching PR merges.

## 9. Out of scope and follow-ups

- **Webhooks.** GitHub exposes the right events (`issues`, `projects_v2_item` with `edited.changes.field_value`), but delivery requires either `gh webhook forward` (CLI-bound, beta transport) or a hosted relay. Polling at 30–60s is fine for human-scale workflows; webhooks are a *separate spike* once polling cadence proves insufficient. File ticket: "Spike: webhook integration for sub-second sync".
- **`rl sync pull --all`.** Doesn't exist today. Useful as the manual escape valve for archived tasks. File ticket: "feat(sync): add `rl sync pull --all`".
- **Per-repo / per-workspace TOML preferences.** Declarative config (e.g. `repo-link.toml`) for per-repo creation preferences — for example, "in workspace `W`, tasks default to being filed in repo `acme/backend`" or "always create as a draft regardless of project link." Out of scope for v1 (the orphan-vs-anchored split in §D1 covers the common cases without config). File ticket: "Spike: TOML-based per-repo preferences for task creation."
- **Project priority + size fields.** The live `repo-link` project has Priority (P0/P1/P2) and Size (XS-XL) single-select fields too. Same machinery as Status — punt to a follow-up RFC.
- **Cross-repo `transferIssue`.** Moving an already-real issue between repos via `rl task edit --repo`. Rejected in v1; needs its own design (cross-org permissions, label remapping).
- **GitHub App auth (JWT).** `octocrab` feature `jwt-rust-crypto` stays off; separate PR if ever needed.
- **GitLab / Gitea adapters.** Port surface is vendor-neutral; sibling adapter crates would be unaffected.

## 10. Open questions

These don't block Stage 1 (REST → octocrab swap) — they only need to be answered before Stage 4.

1. **Where does `application-project` register `ProjectService`?** Same composition root as `TaskService` (the daemon and CLI both wire it). No real choice — calling out for symmetry.
2. **Outbox ordering guarantees.** FIFO per task? Strict global FIFO? Lean: FIFO per task (a single-task `start → edit → complete` sequence must apply in order), but parallel across tasks (a stuck mutation on task A shouldn't block task B). Stage 6 makes this concrete.
3. **Many-to-one mappings** in `project_status_mappings`. If the auto-derivation maps two `TaskStatus` values to the same option (e.g. a 2-state board has only "Open" and "Done"), what happens? The schema represents this natively — two `(project_id, status)` rows sharing one `option_id` — so we store the collision as-is; surface a note on `rl project show`; let the user re-target via `rl project map`. (The earlier scalar `default_for` column couldn't store this and silently dropped the second mapping; see #80.)
4. **Project archival semantics.** A GitHub project can be closed/archived from the UI. **Decision: archival is cosmetic only — no cascade.** We mirror the `archived` flag on the local `projects` row and surface it on `rl project show` / `rl workspace show`, but: child workspaces are unaffected, polling continues (the user may un-archive), and existing outbox entries still drain. The flag is purely a hint for the human reader.
5. **Unlinking a project with active orphan-drafts.** If a user runs `rl project unlink <p>` while orphan tasks (`repo_id = NULL`, `project_item_id IS NOT NULL`) exist under workspaces attached to that project, the drafts on GitHub aren't affected — but the local task loses its only sync anchor (no repo, no project to track via). Options: (a) refuse the unlink until those tasks are resolved, (b) auto-detach drafts (keep them locally as orphan + projectless, no further sync), (c) prompt per task. Lean: (b), with a `rl project unlink --force` to skip prompting and a summary of affected tasks. Decide before Stage 8.

## Appendix A: GraphQL shapes we care about

Confirmed against `api.github.com` on 2026-05-26 via `gh api graphql`.

### Read a project's schema (per `rl project link`)

```graphql
query($owner: String!, $number: Int!) {
  user(login: $owner) {
    projectV2(number: $number) {
      id
      number
      title
      owner { ... on User { login } ... on Organization { login } }
      fields(first: 50) {
        nodes {
          __typename
          ... on ProjectV2SingleSelectField {
            id
            name
            options { id name }
          }
        }
      }
    }
  }
}
```

If multiple single-select fields exist, prefer the one literally named "Status"; fall back to the first single-select.

### Poll for delta (per project, every tick)

```graphql
query($projectId: ID!, $query: String!, $first: Int = 100) {
  node(id: $projectId) {
    ... on ProjectV2 {
      items(first: $first, query: $query) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          updatedAt
          fieldValueByName(name: "Status") {
            ... on ProjectV2ItemFieldSingleSelectValue { optionId name }
          }
          content {
            ... on Issue {
              id
              number
              title
              body
              state
              repository { nameWithOwner }
              assignees(first: 10) { nodes { login } }
            }
          }
        }
      }
    }
  }
}
```

Pass `query: "is:open updated:>$last_poll"` (RFC 3339). For wide-window catch-up after offline: `updated:>$last_known_tick - 5m`.

### Add an issue to a project (lazy, on first push)

```graphql
mutation($projectId: ID!, $contentId: ID!) {
  addProjectV2ItemById(input: { projectId: $projectId, contentId: $contentId }) {
    item { id }
  }
}
```

### Set an item's status (every lifecycle transition for mirror tasks)

```graphql
mutation($projectId: ID!, $itemId: ID!, $fieldId: ID!, $optionId: String!) {
  updateProjectV2ItemFieldValue(input: {
    projectId: $projectId,
    itemId: $itemId,
    fieldId: $fieldId,
    value: { singleSelectOptionId: $optionId }
  }) {
    projectV2Item { id }
  }
}
```

### Live data sample (this account, 2026-05-26)

```text
Project #3 "repo-link" (PVT_kwHOAukuJ84BYZR7) — linked to benediktms/repo-link
  Status field PVTSSF_lAHOAukuJ84BYZR7zhTfceU:
    f75ad846  Backlog
    e18bf179  Ready
    47fc9ee4  In progress
    aba860b9  In review
    98236657  Done
```

## Appendix B: Octocrab capability map

| Requirement | Octocrab mechanism | Notes |
|---|---|---|
| GraphQL queries + mutations | `octocrab.graphql(query)` → `GraphqlResponse<T>` | Generic over response type |
| Compile-time-typed GraphQL | `graphql_client` crate + `#[derive(GraphQLQuery)]` against checked-in `.graphql` schema | Catches schema drift at build time |
| REST issue endpoints | `octocrab.issues(owner, repo)` typed handlers | Replaces our `rest.rs` 1:1 |
| REST pagination (`since`) | `Page<T>` + `octocrab.get_page(&page.next)` | Idiomatic loop |
| ETag / `If-Modified-Since` | `OctocrabBuilder::cache(InMemoryCache::default())` for transparent conditional caching; `events().etag(prev)` per-call where needed | 304s don't decrement rate limit |
| Retry on 429 / secondary limits | `retry` feature flag | Tower-based middleware |
| Tracing spans | `tracing` feature flag | Integrates with PR #38's subscriber |
| PAT auth | `.personal_token(token)` | Same shape as today |
| Header inspection | `_get`/`_post` return raw `http::Response` | Escape hatch |
| **Projects v2 typed API** | **Not available** — `octocrab::projects` is v1 (legacy). All v2 goes through `octocrab.graphql()` | This is a GitHub design constraint, not an octocrab limit |

Feature set we use:
```toml
octocrab = { version = "0.x", default-features = false, features = [
  "rustls", "tracing", "retry", "stream", "timeout",
  "follow-redirect", "default-client",
] }
```

## Appendix C: Dependency cost

Adopting `octocrab` adds, transitively (estimated from a probe of its default-features-off graph): `hyper-util`, `tower`, `tower-http`, `hyper-rustls`, `hyper-timeout`, `futures-util`, `bytes`, `http-body-util`, and a handful of micro-crates. Cold-compile budget: ~5–8s on an M-class laptop; incremental rebuild post-swap: negligible.

`reqwest` stays in `Cargo.lock` (octocrab uses it under `default-client`). No source-level reference to `reqwest` remains in `infra-github` after Stage 1.

Adopting `graphql_client` adds: `graphql-parser`, `graphql_client_codegen`, `proc-macro2`/`syn`/`quote` (already in the workspace via serde derive).

---

**Reviewer notes.** This RFC is the spike's output — design, not code. Implementation lands in the eight staged PRs in §5. Comments and dissent on the decisions go in [#28](https://github.com/benediktms/repo-link/issues/28) before any of S1–S8 starts.
