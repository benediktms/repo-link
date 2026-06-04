# RFC 0004 — Task status, project-status cache, and read-freshness model

Status: Draft (2026-06-04)
Tracking epic: **#TBD**
Supersedes: none
Amends: RFC 0001 §3 D2 ("Authoritative state ownership: mirror + outbox"),
RFC 0001 §3 D4 ("Project axis"), RFC 0001 §5 Stage 8 ("Project status
reads/writes + drift surfacing"), RFC 0003 §2 D7 ("Status stays
outbound-only"), the surface area of `TaskStatus` and the project-status
cache as currently shipped.

## 1. Context

The current model is doing two jobs with one enum, hiding a freshness
decision inside an offline-first read, and conflating "we have a
remote representation of this task" with "we recently observed the
remote's state."

### The defect

**The 5-state `TaskStatus` is doing display work.** The aggregate
carries `TaskStatus::Open | InProgress | Blocked | Done | Archived`,
but the GitHub model is binary (open/closed) plus a `state_reason`
(`completed` | `not_planned` | `reopened`). The local enum was
extended over time to model:

- **Lifecycle** (Open → InProgress → Done) — the REST open/closed
  bit, expressed as 3 distinct values.
- **Workflow** (Blocked) — a meaning layered on top of lifecycle.
- **Editoriality** (Done vs Archived) — a distinction the GitHub
  model carries as `state_reason`, not as a separate state.

The lifecycle-vs-workload split forces a one-to-one mapping
(`TaskStatus::Blocked` ↔ a board option named "Blocked") that the
GitHub model doesn't have. The `Blocked → Open` fallback rule in
`Project::resolved_option_id_for` is a workaround for boards that
don't define a Blocked option, and a hand-rolled `Blocked` mapping
in the link-time seeder. The rule exists because the workflow state
was overloaded onto the lifecycle.

**The `synced_at` decision is currently the read-through path, not
the write-through path.** Project status is rendered on `task show`
from a cached value (`task.project_status_option_id`) written by the
poller. The cache is "fresh" if the poller has run recently; it's
"stale" if the user is looking at the task more often than the
poller's interval. Today there is no explicit way to ask "is this
fresh?" and no explicit way to force a refresh.

**`ConflictKind::StatusMismatch` exists but is never constructed or
read.** It's a placeholder for a design that didn't ship. Its
presence is a tripwire: future engineers will be tempted to wire
it up. The new model makes that temptation wrong by deleting the
variant.

### Three things being conflated

The current surface is hard to reason about because three different
"is the project status fresh" questions are answered with the same
mechanism (or no mechanism at all):

1. **For the poller:** "is this task's board column stale enough to
   re-fetch?" — answered today by a per-project `updated_at`
   watermark and a per-task in-memory record, not persisted.
2. **For the read-through:** "should `task show` re-fetch before
   rendering?" — answered by *not* re-fetching; the read is
   offline-first. Stale data is silently served.
3. **For the push path:** "did our last write land on the remote?"
   — answered today by the drainer treating the write as
   fire-and-forget. The `Result<(), PortError>` from the port is
   logged, not inspected for response state.

The new model gives each consumer its own answer, with a single
shared column (`synced_at`) and a single shared query helper.

## 2. Decisions

### D1 — `TaskStatus` becomes `is_open` + `state_reason`; blocked is derived from relations

The `Task` aggregate drops the `TaskStatus` enum. In its place:

- `is_open: bool` — the open/closed bit. Maps 1:1 to the REST
  `state` field.
- `state_reason: Option<StateReason>` — `Completed | NotPlanned |
  Reopened`, set when `is_open = false` (or, for `Reopened`, when
  transitioning back to open). Maps 1:1 to the REST `state_reason`.
- `blocked_by: Vec<TaskId>` — the existing relation edges. Already
  on the aggregate. The new "is this task blocked?" predicate is
  `!task.blocked_by.is_empty()`.

The `is_blocked()` predicate is derived, not stored. There is no
`TaskStatus::Blocked` variant. The `Blocked → Open` fallback in
`Project::resolved_option_id_for` is deleted. The `Blocked` row in
the link-time auto-derivation seed is deleted. The CLI verb
`rl task block` / `rl task unblock` is removed — blocking is
expressed as a relation (`rl task relate <this> blocked_by <that>`).

State-machine guards on `set_status` transitions are removed. The
lifecycle is editorial: any state can transition to any other state
without an enforced order. **Invariants** that remain:

- A task with `is_open = false` MUST have a `state_reason` (a
  closed task is closed *for a reason*). The valid set is
  `Some(Completed | NotPlanned)` (an `is_open = false` task with
  `state_reason = None` is malformed).
- A task with `is_open = true` MUST NOT have
  `state_reason = Some(Completed | NotPlanned)` (open tasks cannot
  carry a terminal-closure reason). A task with `is_open = true`
  MAY have `state_reason = Some(Reopened)` (the marker for the
  *transition* from closed to open, distinct from "open since
  creation") or `state_reason = None`.
- A `LocalOnly` task MUST NOT have `task.remote.is_some()` (the
  `promote_to_remote` transition is the only path that sets remote).
- A task referenced in any `blocked_by` relation MUST exist
  (referential integrity, not a state invariant — the storage layer
  enforces this; the aggregate layer is the safety net).

Everything else (e.g. "you can mark a `Blocked` task as `Done`
directly") becomes a valid transition.

### D2 — Display DTO is opaque; freshness annotation only on mirrored tasks

The DTO is no longer a 1:1 mirror of the aggregate. A new layer
(`derive_display_status(&Task, relations: &[TaskRelation], &Project) -> DisplayStatus`) composes:

- `is_open` + `state_reason` → the lifecycle label.
- `blocked_by` (queried or carried) → "Blocked (by #N)" when non-empty.
- `project_status_option_id` + `synced_at` → the cached board column
  (with a freshness annotation).

The freshness annotation is rendered on the DTO **only** when
`is_mirror(task) == true` (i.e. `task.sync != LocalOnly` — covers
both issue-backed and draft-backed mirrors, per the existing
`is_mirror` predicate in `application-sync/src/enqueue.rs`). A
`LocalOnly` task has no remote representation, so no
"Last refreshed: …" line — the field is absent, not "Never refreshed."

For mirrored tasks, the DTO shows the `synced_at` timestamp
verbatim: "Last refreshed: 30s ago" / "Last refreshed: 2h ago." No
boolean. The timestamp *is* the staleness signal.

A `DirtyLocal` task shows the freshness line, but the dirty signal
takes precedence in the display order. A `Conflict` task shows both
the conflict reason and the freshness line.

The display helper takes the relations slice as a parameter (not as a
field on `Task`): `derive_display_status(&Task, relations: &[TaskRelation], &Project) -> DisplayStatus`.
The relations slice is the *same* data the relations engine filters by
`RelationKind::BlockedBy` to answer "is this task blocked?" — the
display and the predicate share one query. The helper is a pure
function; the service that composes it does the relations query.### D3 — Write-through `synced_at`; per-workspace active gate; poller is the only SQL filter

The `tasks` table gains a `synced_at: Option<Timestamp>` column. The
column is genuinely fresh — no prior column carried this signal. The
existing `project_status_option_id` cache is a value-only column
(`20260529000004_task_project_status_cache.sql:15`) with no
companion timestamp. The closest pre-existing "last synced" signal
is `remote_mappings.last_synced_at` (a per-`(task, repo, remote_id)`
row, not per-task) — semantically different and not a valid
backfill source. **The migration is therefore a pure `ADD COLUMN
synced_at` with no backfill.** Every existing task starts with
`synced_at = NULL`, meaning "never observed." The poller will
re-fetch every mirrored task on its first post-migration tick (one
burst, then steady state) — acceptable given the 5min cadence.

**Write-through only.** The `synced_at` column is stamped by three
writers, each gated on a *successful network response*:

- **The pull path** — after a successful `fetch_remote` + apply.
  The response is the new state; we observed it directly.
- **The drainer** — after a successful apply of an outbound
  mutation. The disposition of the response (see D5 below) is
  what determines whether the stamp fires.
- **The poller** — after a successful per-task fetch on its tick.
  The fetched `RemoteTaskSnapshot` is the new state; the poller
  observed it directly.

**No eager local stamps.** A local edit that hasn't yet been pushed
does not stamp `synced_at`. The next poll or `--refresh` is what
catches up.

**No re-baseline from the response.** The drainer may read the
response to confirm success, but the baseline is re-baselined
through the existing `confirm_synced_fields` mechanism (RFC 0003 §2
D5) — never from the PATCH response (which reflects `assignees=[]`
when the PATCH didn't set them, per D5).

**Poller is the only SQL filter on `synced_at`.** The poller uses
it to decide per-task whether to fetch. Other readers (the DTO
freshness annotation, the `--refresh` flag's threshold check, the
failed-refresh annotation) read the column as a per-row projection
on an already-loaded task — no JOIN, no extra query. The unified
abstraction is `synced_at + budget < now() -> needs_refetch`, with
the `Duration` budget passed by each caller. The named budgets
(poller: 5min; `--refresh`: 60s) live as `pub const` next to the
function.

```sql
SELECT tasks.* FROM tasks
JOIN workspaces ON workspaces.id = tasks.workspace_id
WHERE workspaces.status = 'active'
  AND tasks.project_item_id IS NOT NULL
  AND (tasks.synced_at IS NULL OR tasks.synced_at < ?)
```

The `?` is the poller's tick boundary — `now() - poll_budget`. Tasks
recently stamped (by any of the three writers above) are skipped on
the next tick.

The query is implemented as a **`TaskFilter` extension**, not as a
new in-memory helper. The poller already has an explicit
`TODO(scale)` to add a project-item predicate to `TaskFilter`
(because the current in-memory filter is O(all tasks) on every
tick). Phase 4 of the migration adds
`TaskFilter { synced_at_lt: Option<Timestamp>, has_project_item_id: bool, ... }`,
closes the poller's existing TODO, and reuses the new SQL filter
for both the poller and any future per-task query that needs the
same shape.

**Per-workspace gate.** The poller skips tasks whose workspace is
not `WorkspaceStatus::Active`. A workspace in `Created` (initial
state, never reached in practice for project-attached workspaces),
`Paused` (user paused), or `Archived` (terminal) is not polled.
`WorkspaceStatus` is a 4-variant state machine today
(`Created | Active | Paused | Archived`); the poller gate is
`status = 'active'` only. A future `Deleted` variant (e.g. for
detached workspaces) must extend the gate explicitly — the tripwire
test asserts the JOIN clause filters on the active status, so a
silent regression where `Deleted` is omitted will fail the test.

**Thresholds and rate-limit math.** Two budgets, named:

| Caller | Budget | Rationale |
|---|---|---|
| Poller (background) | 5 min default | 1,200 fetches/hour for a 100-task workspace (12 polls/hour × 100 tasks); well within GitHub's 5,000 req/h rate limit. 5 min budget supports up to ~415 tasks per workspace at steady state (415 × 12 = ~4,980 fetches/hour); larger workspaces need adaptive budgets (**shipped: `LIMIT` 200 per tick to bound burst cost; out of scope: per-workspace adaptive budget — flagged for followup**). |
| `task show --refresh` (on-demand) | 60s default | "I just looked at this; if I look again 30s later, show me the same thing; if I look 90s later, refresh." Tuned for user-experience, not batch budget. |

A poller `LIMIT` (default 200 per tick) prevents the post-migration
burst from OOM-ing on a 1000-task workspace and bounds the steady-
state cost. Tasks beyond the `LIMIT` defer to the next tick. The
flag is a guardrail, not a per-workspace budget.

### D4 — `rl task show` stays offline; `--refresh` is the explicit opt-in to network

`rl task show <id>` is offline by default. It renders the cached
value (or the local-only state for non-mirrored tasks). No network
I/O. The freshness annotation, when present, shows the
`synced_at` value — the *last time the poller or pull or push
observed the remote*.

`rl task show <id> --refresh` is the explicit opt-in to a network
fetch. The flag:

1. Calls `fetch_remote` for the task.
2. On success: stamps `synced_at` (the D3 disposition is `Stamped`),
   renders the fresh state.
3. On failure (network error, rate limit, GitHub down): renders the
   cached value with the freshness annotation marked as "Last refresh
   failed at <timestamp>: <error>." Does NOT propagate the error.

The flag is a *flag*, not a separate verb. There is no
`rl task refresh <id>` standalone command — the verb form is
overloaded with `rl sync pull` (which has different semantics:
`--refresh` does not overwrite local title/body/assignees; `sync
pull` does). Keeping them distinct via the flag form preserves the
distinction without expanding the verb surface.

`rl task block` / `rl task unblock` are removed. Blocking is now
expressed via `rl task relate <this> blocked_by <that>`. The
`start` / `complete` / `archive` verbs survive as direct setters
on `is_open` + `state_reason`.

### D5 — Drainer response disposition is a 3-state machine; not "stamp vs. no-stamp"

The drainer's `apply` method for outbound mutations currently
treats the port's `Result<()>` as fire-and-forget. The new model
adds a **3-state disposition** for every outbound mutation:

```rust
enum ApplyDisposition {
    /// Response confirms the sent payload (or is HTTP success for
    /// arms with no response body). Stamp `synced_at` and mark
    /// the outbox row Succeeded.
    Stamped,
    /// Transient failure (5xx, 429, network error). Existing
    /// retry/backoff path. No stamp.
    Retry,
    /// Response succeeded but disagrees with the sent payload
    /// (e.g. `option_id` mismatch, `assignees` empty when not
    /// set, 4xx semantic). Transition task to `SyncState::Conflict`,
    /// surface in `rl query drift`, do NOT stamp. The outbox
    /// entry is dead-lettered (no retry — the conflict is
    /// real, not transient).
    Conflict,
}
```

**`OutboxMutation` has 10 variants** (per
`crates/domain-sync/src/outbox.rs`); the drainer has 10
corresponding `apply` arms. The disposition per arm:

| Variant | Response shape | Stamped iff | Disposition tripwire |
|---|---|---|---|
| `UpdateRemote` | `RemoteTaskSnapshot` (REST `update_issue` returns the updated issue) | `response.assignees == sent.assignees \|\| response.assignees == []` (D5 carve-out; see D3 — "PATCH response reflects `assignees=[]` when the PATCH didn't set them") | injects a port that returns wrong `assignees`; asserts `Conflict` |
| `AddItem` | new `project_item_id: String` | `project_item_id.is_some()` | injects a port that returns `None`; asserts `Conflict` |
| `CreateDraftIssue` | new `project_item_id: String` | `project_item_id.is_some()` | same as `AddItem` |
| `UpdateDraftIssue` | `()` (fire-and-forget port) | HTTP success (no body to compare) | injects a port that returns `Err`; asserts `Retry` |
| `ConvertDraftToIssue` | `(issue_node_id, issue_number)` | both fields parse as `I_*` and a positive integer | injects a port that returns junk; asserts `Conflict` |
| `SetProjectStatus` | new field value (GraphQL) | `response.option_id == sent.option_id` | injects a port that returns the wrong `option_id`; asserts `Conflict` |
| `AddSubIssue` | `()` (relation-sync, REST 204) | HTTP success | injects `Err`; asserts `Retry` |
| `RemoveSubIssue` | `()` (relation-sync, REST 204) | HTTP success | injects `Err`; asserts `Retry` |
| `AddBlockedBy` | `()` (relation-sync, REST 204) | HTTP success | injects `Err`; asserts `Retry` |
| `RemoveBlockedBy` | `()` (relation-sync, REST 204) | HTTP success | injects `Err`; asserts `Retry` |

The 4 relation-sync arms (`AddSubIssue` / `RemoveSubIssue` /
`AddBlockedBy` / `RemoveBlockedBy`) return `()` today. Their
disposition is "HTTP success → Stamped" — there is no response
content to disagree on, so the tripwire for these arms is "Err
→ Retry", not "wrong response → Conflict." A future port that
returns a response body for these arms can be retro-fitted to the
inspectable pattern.

**`UpdateDraftIssue`** is the one write-mutation with no response
content; it stamps on HTTP success alone. This is the right
behaviour — the alternative (fire-and-forget without stamp) would
mean every draft update leaves the cache stale, and the poller
re-fetches on every tick.

**`mark_synced` is the single write path.** All 10 arms funnel
through `TaskRepository::cache_synced_at(task_id, ts, source: SyncedSource)`
— a new repository method next to the existing
`cache_project_status` and `cache_remote_node_id`, both of which
established the "single-column UPDATE, no whole-row save" pattern.
The `SyncedSource` enum is `Pull | Push | Polled`; a debug-mode
assertion pins the source to the call site (drainer arms can only
stamp `Push`, poller can only stamp `Polled`, pull path can only
stamp `Pull`). The shape of `cache_synced_at` is `UPDATE tasks SET
synced_at = ? WHERE id = ?` — same rationale as
`cache_project_status` (no whole-row clobber, no version bump, no
snapshot append, no concurrent-CLI-edit race).

**`Conflict` transitions need a `ConflictKind`.** A `SetProjectStatus`
whose response returns the wrong `option_id` is a real
project-status conflict, not an `AssigneeMismatch` or a `LocalEditedRemoteEdited`.
The new `ConflictKind::ProjectStatusMismatch` (or
`ConflictKind::ProjectItemMismatch`, depending on the
mutation-specific semantics) is added alongside the existing
variants. `ConflictKind::StatusMismatch` is deleted — see the
"Dead variant" entry in the "Risks" section for the rationale and
the doc-comment that must be added to `ConflictKind` itself to
explain the gap.

## 3. Non-goals

- **D8 (labels)** — same status as before. Deferred to a followup
  RFC.
- **Auto-resolution of project-status drift** — the drift surface
  (`query drift` reports `reasons = ["project_status"]` when local
  lifecycle disagrees with cached remote column) is unchanged. The
  new model makes the surface fresher (the poller is gated on
  `synced_at`, not a fixed watermark), but no auto-resolution is
  added.
- **Polymorphic `blocked_by`** — relations point at real local
  tasks only. An external dependency (a blocker that isn't a
  tracked task) would require either a placeholder task locked to
  `LocalOnly` or a polymorphic join table. Both are separate
  features, deferred.
- **Configurable thresholds** — defaults 5min poller / 60s
  `--refresh`. Per-workspace or per-user overrides are a followup.
  The RFC names the rate-limit math (5min × 100 tasks = 12
  fetches/hour) so the thresholds have rationale.
- **Auto-pause on workspace inactivity** — the per-workspace gate is
  explicit (`WorkspaceStatus::Paused`), not inferred from
  inactivity. Out of scope.

## 4. Alternatives considered

- **Keep `TaskStatus` as-is, just add `--refresh`.** Rejected: the
  display problem (the 5-state enum doing both lifecycle and
  workflow) and the freshness problem (the offline-first read
  hiding staleness) are independent. `--refresh` alone fixes the
  freshness half. Dropping `TaskStatus` is the load-bearing change
  for the display half.
- **Read-through on `task show` (the previously-considered design).**
  Rejected: the CLI is offline-first by design (RFC 0001 §3). The
  network-on-read model has O(N) latency for listing views, no
  rate-limit math, and forces the user to wait on every read. The
  write-through + opt-in `--refresh` model preserves the offline
  read and gives the user an explicit verb.
- **Two `synced_at` columns (one for project status, one for the
  full remote snapshot).** Rejected: only the project status is
  cached today. A future "I want to cache comments too" can add a
  second column; one column per axis is easier to reason about
  than one column with a flag.
- **Drop the poller; rely on `--refresh` exclusively.** Rejected:
  cold-start would re-fetch every task on first view. The poller is
  the freshness source for tasks nobody is looking at; the
  read-through is the freshness source for tasks the user is
  actively looking at. Both are needed.
- **Activate `ConflictKind::StatusMismatch`.** Rejected: the design
  doesn't ship it for the same reason it didn't ship the first
  time — there's no faithful inverse from GitHub's two-state
  open/closed onto the local lifecycle. The variant is deleted.
- **The poller does a single `updated:>` floor without a per-task
  decision.** This is the status quo. Rejected: a remote move on
  one task forces the poller to re-fetch every task in the same
  poll window, because the API returns items by `updated_at`, not
  per-task freshness. The per-task `synced_at` lets the poller
  *filter* its batch, not just its API call.

## 5. Risks

- **The drainer response inspection is a real refactor.** Today
  the drainer's `apply` arms do `?` on the port result and move
  on. The new model requires reading the response and computing
  a 3-state disposition (`Stamped` / `Retry` / `Conflict`). The
  risk: a future mutation variant (e.g. a new RFC 0003 D8
  labels push) forgets the disposition machine and silently
  regresses to fire-and-forget, re-introducing the "stamp
  without confirming" bug. Mitigation: a per-arm tripwire
  test that injects a port returning the wrong response and
  asserts the right disposition. The 10 arms have 10
  tripwires, one per row of the D5 table.
- **Deleting `ConflictKind::StatusMismatch` removes the
  in-code tripwire against re-deriving the wrong shape.** The
  variant is currently never constructed or read; deleting it
  is a 1-line change. The risk: a future engineer who needs
  to model "local lifecycle vs. remote open/closed disagreement"
  will not know the previous design was considered and
  rejected, and may re-derive the wrong shape. Mitigation: a
  doc-comment on the `ConflictKind` enum itself explaining
  the gap and pointing at RFC 0004 D1 + RFC 0003 §6 OQ5.
- **The `synced_at` semantics drift.** Today it's the
  "project-status cache freshness" stamp; post-RFC it's the
  "remote-observation freshness" stamp. Any code that reads
  `synced_at` and assumes the narrow reading (e.g. "this is the
  last time the project status was refreshed, not the last time
  anything was observed") will be wrong. Mitigation: rename +
  backfill, plus a search-and-replace for any consumer that
  read the old column.
- **The per-workspace gate interacts with workspace state
  transitions.** When a workspace is paused, its tasks' `synced_at`
  is not touched. The tasks stay "fresh" (stale column, fresh stamp)
  even though the poller will skip them. When the workspace is
  unpaused, the poller will skip them again until the stamp ages
  out (or the user runs `--refresh`). The user has no signal that
  the resume-on-unpause has a delay. Mitigation: the RFC documents
  the behavior; a followup could add a "force re-poll on unpause"
  if it becomes painful.
- **The thresholds are not configurable.** A workspace with 1000
  tasks at the 5min default would consume 12000 fetches/hour —
  over GitHub's rate limit. The RFC names this in the testing
  strategy but doesn't ship a fix. Mitigation: a followup ticket
  for per-workspace rate budgets; not a blocker.
- **The DTO freshness annotation wording.** "Last refreshed: 30s
  ago" is clear, but the wording for "refresh failed" or "never
  refreshed" is TBD. Small UX risk; the design is honest either way.
- **Clock skew.** `Timestamp` is `DateTime<Utc>` from `chrono`
  (wall-clock, not monotonic). A backwards jump (NTP correction,
  manual `date -s`, VM clock drift) would make `synced_at` appear
  *in the future* relative to `now()`, so the threshold check
  `synced_at + budget < now()` always returns true and the
  poller re-fetches every task on every tick. A forward jump
  (e.g. NTP slew) makes the threshold fire too late, leaving
  stale data longer than expected. Mitigation: the freshness
  comparison uses `std::time::Instant` (monotonic) for the
  *delta*, stored alongside `Timestamp` (wall-clock) for the
  *display*. The schema column is `synced_at: Timestamp`; the
  in-memory `Task` carries a `synced_instant: Option<Instant>`
  for threshold checks. On process restart, `synced_instant` is
  `None` (treated as stale) — the poller's first tick refreshes
  everything, which is the same shape as the post-migration
  burst. Alternative mitigation (out of scope): clamp negative
  deltas to "stale" (force refresh) and treat a forward jump
  exceeding the threshold as "stale" too.

## 6. Testing strategy

### Tripwires (compile-time + runtime)

- **`TaskStatus` enum deletion:** a test in `domain-task` that
  greps the workspace for `TaskStatus::` and asserts zero matches.
  Catches any reintroduction.
- **`synced_at` stamp discipline:** a test in `application-sync`
  that any code path stamping `synced_at` must be in a small set
  (the three writers). Achieved by a `mark_synced(&mut task,
  source: SyncedSource)` helper that takes a `SyncedSource` enum
  variant (`Pull | Push | Polled`) and asserts the source at
  runtime if a `cfg!(debug_assertions)` flag is on. The helper is
  the only way to write the column.
- **Poller gate:** a test in `application-sync::poller` that
  asserts the SQL query filters on `workspaces.status =
  'active'`. A future refactor that drops the JOIN fails the
  test.
- **Drainer response inspection:** a test that injects a port
  that returns a response inconsistent with the sent mutation
  (e.g. wrong `option_id`) and asserts the drainer does NOT stamp
  `synced_at`. Per D5.

### Unit tests

- **`is_blocked()` derived correctly:** empty `blocked_by` → false;
  non-empty → true. Tests at the aggregate layer.
- **Invariants:** the four named invariants (closed-with-reason,
  not-planned-cannot-be-open, etc.) get unit tests in
  `domain-task`. The aggregate constructor or field-setter
  enforces them; the test asserts the error variant.
- **Threshold helper:** `staleness_threshold(ReadThrough) == 60s`,
  `staleness_threshold(Background) == 5min`. Tests the shared
  function, not the per-caller sites.
- **`tasks_needing_sync` helper:** the per-task filter returns
  tasks with `synced_at` older than the threshold; recently
  stamped tasks are skipped; `None` is "needs sync."

### Integration tests

- **Read-through flag:** `task show --refresh` calls `fetch_remote`,
  stamps on success, falls back on error. The test uses a fake
  provider that can be configured to succeed or error.
- **Offline behavior:** `task show` (no flag) does NOT call
  `fetch_remote`. The test asserts the fake provider's
  `fetch_remote` count is zero.
- **Poller integration:** the poller's tick, with a mix of fresh
  and stale tasks in a mix of active and paused workspaces,
  fetches only the stale + active subset. A test seeds an
  in-memory `WorkspaceRepository` and `TaskRepository` and asserts
  the fetch count.
- **Drainer response inspection:** as above, with a fake port that
  returns the wrong `option_id`; the drainer logs the mismatch
  and does not stamp. *Three* tests, not one: (a) `Stamped` —
  port returns the expected response; assert `synced_at` was
  stamped and the outbox row is `Succeeded`; (b) `Retry` — port
  returns `Err`; assert no stamp and the outbox row is
  rescheduled with backoff; (c) `Conflict` — port returns
  success but the response disagrees with the sent payload;
  assert no stamp, the outbox row is dead-lettered, and the
  task's `sync_state` is `Conflict`. Per the D5 table, each
  of the 10 `OutboxMutation` variants has a tripwire of one
  of these three shapes.

## 7. Open questions

The questions below are *genuinely open* — the RFC does not imply
an answer. (Risks and non-goals cover the rest.)

1. **CLI verb for batch refresh.** `task show --refresh` exists;
   `rl task refresh-all` (refresh every mirrored task in one
   batch) does not. The followup is small but needs user
   feedback to scope (rate-limit interaction with the poller, UX
   of progress reporting, error handling on partial failure).
2. **Wording for failed `--refresh` annotation.** The DTO will
   carry the data ("last refresh failed at <ts>: <error>");
   the user-facing wording is TBD. Ship the data, refine via
   UX feedback.

## 8. Migration plan

### Phase 1 — Schema + aggregate (no behavior change)

- DB migration: add `tasks.synced_at: Option<Timestamp>`. **No
  backfill** — the pre-existing schema has no per-task
  project-status timestamp column (`project_status_synced_at`
  does not exist on `tasks`; the closest is
  `remote_mappings.last_synced_at`, which is a different axis).
  Every task starts with `synced_at = NULL` meaning "never
  observed." The poller's first post-migration tick re-fetches
  every mirrored task (one burst, then steady state) — acceptable
  given the 5min cadence.
- Domain: `Task` aggregate gains `is_open`, `state_reason`,
  `relations: &[TaskRelation]` (already present). Removes the
  `status: TaskStatus` field. New constructors and setters.
  **~140 call sites refactored** (not "~30" — see the in-code
  references for a count; the high-traffic sites are
  `lifecycle_to_remote_state`,
  `MirrorPatch::status`, `MirrorField::Status`,
  `TaskFilter::status: Some(TaskStatus::Blocked)`,
  `reconcile_dirty_against_baseline`,
  `diff_against_baseline`, the 4 tripwire tests in
  `domain-task`, and `blocked_tasks` / `ready_tasks` in
  `application-query`).
- Repository: new `TaskRepository::cache_synced_at(task_id, ts,
  source: SyncedSource)` method next to `cache_project_status`
  and `cache_remote_node_id`. Single-column UPDATE, no
  whole-row save. The `SyncedSource` enum is `Pull | Push |
  Polled`; a debug-mode assertion pins the source to the
  call site.

### Phase 2 — Display layer

- New `derive_display_status(&Task, relations: &[TaskRelation], &Project) -> DisplayStatus`
  in `application-task` (or wherever the DTO is composed).
- `TaskDto` carries the new fields. CLI rendering updated.
- The freshness line is rendered only on `is_mirror(task) == true`.

### Phase 3 — CLI

- `rl task show --refresh` flag added.
- `rl task block` / `rl task unblock` removed.
- `rl task start` / `complete` / `archive` updated to call the
  new direct setters.

### Phase 4 — Poller

- Extend `ports::TaskFilter` with `synced_at_lt: Option<Timestamp>`
  and `has_project_item_id: bool`. The poller's per-tick query
  becomes a SQL-level filter (replacing the in-memory
  `index_tasks_by_item_id` filter and its `TODO(scale)`); the
  `WHERE` clause is JOIN on `workspaces` + `status = 'active'` +
  `has_project_item_id` + `synced_at_lt`. Add a composite index
  `CREATE INDEX idx_tasks_poll ON tasks(workspace_id,
  project_item_id, synced_at) WHERE project_item_id IS NOT NULL`
  so the per-tick cost is O(log N + stale_count), not O(N).
- **Per-tick `LIMIT` (default 200):** bounds the post-migration
  burst on a 1000-task workspace and the steady-state cost
  above 400 tasks per workspace. Tasks beyond the `LIMIT` defer
  to the next tick; they are not lost.
- Per-task: on successful fetch, stamp `synced_at` via
  `TaskRepository::cache_synced_at(task_id, ts, Polled)`.

### Phase 5 — Drainer

- For each of the 10 `apply` arms, implement the D5
  `ApplyDisposition` 3-state machine: `Stamped` / `Retry` /
  `Conflict`. The arm-specific tripwire is the D5 table
  (column "Disposition tripwire"); each arm has a unit test
  that injects a port returning the wrong response and asserts
  the right disposition.
- Replace the existing fire-and-forget `?` with a
  `match response { ... -> ApplyDisposition }` block in each
  arm. The new `mark_synced` helper routes through
  `TaskRepository::cache_synced_at`; the disposition machine
  determines whether the cache write fires.
- Add `ConflictKind::ProjectStatusMismatch` (or
  `ConflictKind::ProjectItemMismatch`, depending on the
  mutation's semantics) for the arms that can detect a
  semantic-mismatch. The existing `ConflictKind` enum gains
  the new variant and loses `StatusMismatch` (the deletion is
  a 1-line change; the doc-comment on `ConflictKind` is
  extended to explain the gap and point at RFC 0004 D1).
- **Drainer response inspection scope:** the 4 relation-sync
  arms (`AddSubIssue` / `RemoveSubIssue` / `AddBlockedBy` /
  `RemoveBlockedBy`) return `()` today; their disposition is
  "HTTP success → Stamped". A future port that returns a
  response body for these arms can be retro-fitted to the
  inspectable pattern. The migration does **not** change
  these ports' signatures.
- **Drainer's `enqueue_status_follow_up`** (the lazy
  `AddItem` / `CreateDraftIssue` write-back at
  `drainer.rs:570-602`) enqueues a follow-up `SetProjectStatus`
  after the initial `AddItem` succeeds. The two arms are
  applied serially; each arm's disposition is computed
  independently. A successful `AddItem` followed by a
  failed-`Conflict` `SetProjectStatus` is `Stamped` for the
  first arm, `Conflict` for the second, and the task ends in
  `Conflict`. A future RFC can fold these into a single
  multi-step apply if the failure mode is painful.

### Phase 6 — Tests

- All existing tests that construct `Task`s with `TaskStatus`
  variants are updated to use the new direct setters. The
  compiler is the migration guide.
- New tripwire tests in `domain-task` and `application-sync`
  (per the testing strategy above).
- `rpl-2l6` (the original `task show` display spike) is closed,
  pointing at this RFC.

## Appendix A — current field matrix (post-RFC)

| Field | On `Task` | On `TaskSnapshot` | In detection | In `RemoteTaskUpdate` | Drained from | Polled | DTO source |
|---|---|---|---|---|---|---|---|
| `is_open` | yes | yes | yes (via diff) | yes (closed) | drainer | yes | direct |
| `state_reason` | yes | yes | yes (via diff) | yes (state_reason) | drainer | yes | direct |
| `blocked_by` | yes | no (relation, not field) | n/a (relation, not field) | n/a | n/a | n/a | derived |
| `assignees` | yes | yes | yes | yes (assignees) | drainer | yes | direct |
| `title` | yes | yes | yes | yes (title) | drainer | yes | direct |
| `body` | yes | yes | yes | yes (body) | drainer | yes | direct |
| `project_status_option_id` | yes (cached) | no | no | no | no | yes (write-through) | direct |
| `synced_at` | yes | no | no | no | drainer (on confirmed response) | yes (on tick) | direct |
| labels | no | no | no | n/a (D8 deferred) | n/a | n/a | n/a |
| milestone | no | no | no | n/a | n/a | n/a | n/a |

The detected set (the `MirrorField` set in `domain-task`) reduces
to `{is_open, state_reason, assignees, title, body}`. `blocked_by`
is a relation, not a mirrored field — it travels through the
relations machinery, not the snapshot diff. `project_status_option_id`
and `synced_at` are read-only cache state, not mirrored content.
