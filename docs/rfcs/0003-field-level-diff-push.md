# RFC 0003 — Field-level diff push: baseline-driven mirrored-field propagation

Status: Draft (2026-06-01)
Tracking epic: **#TBD**
Amends: RFC 0001 §3 D2 "Authoritative state ownership: mirror + outbox" (the outbound field set), §5 Stage 6 "Outbox refactor + lifecycle wiring" (the drainer's re-derive-from-live-task contract and the baseline-advance rule), and §6 "Port and schema sketches" (the `RemoteTaskUpdate` / `OutboxMutation::UpdateRemote` shapes).

## 1. Context

repo-link mirrors a local `Task` aggregate to a GitHub Issue (and an optional Projects v2 item). The mirrored-content axis is **title, body, status (as the issue open/closed bit), and assignees**. The system already rests on three working pillars:

- **Append-only snapshot history + last-synced baseline.** `task_snapshots` is a true append-only log (`PRIMARY KEY (task_id, version)`, monotonic version assigned by the repository). Every task write appends one snapshot tagged with its `SnapshotSource`; `save_with_outbox` commits the task row, the snapshot, and any outbox entries in one `BEGIN IMMEDIATE` transaction. On read, the repository hydrates `Task::synced_baseline` with the highest-version baseline-eligible snapshot. `TaskSnapshot` already carries `assignees`. **The last-synced snapshot the team wants to diff against already exists and is loaded into every Task the application layer touches.**
- **Dirty detection (field-complete).** `Task::reconcile_dirty_against_baseline` diffs exactly `title`, `body`, `status`, and `assignees` (the last via order-insensitive `assignees_equal`) against `synced_baseline`. `set_assignees` calls it, so an assignee-only edit correctly flips a `Synced` task to `DirtyLocal`. `priority`, `repo_id`, `filing_repo_id`, comments, and project status are deliberately excluded.
- **Inbound (pull) is assignee-complete.** `SyncService::pull` fetches a `RemoteTaskSnapshot`, computes drift via `summary::remote_mirrors_baseline` (compares title, body, assignees), and on a `PullRemote` decision copies `title`, `body`, and `assignees` back onto the task before re-baselining with `SnapshotSource::Pull`. (Status open/closed is intentionally not copied back — the 5-state lifecycle has no faithful inverse from GitHub's two states.)

### The defect

**Dirty detection is field-complete; dirty *propagation* is field-incomplete and coarse.** Both outbound paths blanket-send a fixed `{title, body, closed, state_reason}` set with no diff against the baseline, and assignees have **no channel at any propagation layer**:

- `ports::RemoteTaskUpdate` carries `title`, `body`, `closed`, `state_reason` — no `assignees`.
- `OutboxMutation::UpdateRemote` carries `title`, `body`, `closed` — no `assignees`.
- `enqueue::plan_mutations` (the canonical enqueue site) hardcodes that set for every issue-backed mirror.
- `OutboxDrainer::apply` (the daemon's sole outbound path) ignores the captured payload, re-reads the live task, re-derives `(closed, state_reason)` from `lifecycle_to_remote_state(task.status)`, and sends the same shape — it never reads `task.assignees`.
- `SyncService::push` (kept only for `rl task claim` interactive feedback, per RFC 0001 §5 Stage 6) builds the same shape inline.
- `RestClient::update_issue` sets only `.title`/`.body`/`.state`/`.state_reason`; it never calls `.assignees()`.

The detected-set minus the propagated-set is exactly **{assignees}**. (Status *is* propagated, lossily, as the open/closed bit.) Assignees reach GitHub exactly once — at create, via `RemoteTaskCreate.assignees` forwarded by `create_issue`. The gap is strictly the post-promote **update** path.

### Why the loss is silent and permanent

This is not merely a dropped write. After an assignee-less push, `confirm_synced(SnapshotSource::Push)` re-baselines `synced_baseline` from a fresh `snapshot_view` that captures the **new, un-pushed** assignees. The next `reconcile_dirty_against_baseline` therefore sees no diff and never re-pushes. The local baseline records a value the remote never received, with no detection backstop.

This is the confirmed root of the bug: `rl task claim` on an already-promoted task sets the local assignee, flips `DirtyLocal`, pushes — and the assignee is dropped, leaving the GitHub issue unassigned. It reproduces identically on both the synchronous claim path and the async daemon drainer.

**Observed incident (RFC 0002 reconciliation, 2026-06-01).** Several `Done` tasks sat in `DirtyLocal`/`Conflict` because local recorded `assignees=[benediktms]` (set by `claim`) while their GitHub issues showed `assignees=[]`. GitHub does not clear assignees on close, so the assignee was simply never pushed. Each task had to be reconciled by hand — `rollback → pull → complete → push` — and the drift detector kept re-flagging drift the outbound path could never resolve. The common trigger: `promote` while unassigned (create sets `[]`) followed by `claim` (assignee only ever travels the update path).

### Three divergent field-set definitions

There is no single source of truth for "what fields mirror." Today:

- detection (`reconcile_dirty_against_baseline`): `{title, body, status, assignees}`
- inbound drift (`remote_mirrors_baseline`): `{title, body, assignees}` (no status)
- outbound update (`plan_mutations` / `update_issue`): `{title, body, status-as-closed}` (no assignees)

The baseline-eligibility predicate is also duplicated — the Rust `SnapshotSource::is_baseline` / `TaskSnapshot::is_baseline` versus the SQL `WHERE` literals in the snapshot repository's baseline load — with no shared source of truth.

### Adjacent gaps (scoping inputs, not all in-scope)

- **Labels** are plumbed only at the endpoints: present on `RemoteTaskCreate` (but always passed `&[]` from promote, so never written) and read by the inbound snapshot mapping (then dropped — no consumer stores them), but **not a `Task` field**, not in `TaskSnapshot`, not detected, not in any update DTO. Connected at both ends, disconnected in the middle.
- **Milestone** is absent at every layer — greenfield.
- The Projects v2 GraphQL poll (`poll_project_items`, RFC 0001 §D4) projects only id/number/title/body/state/Status-option — it does **not** read assignees or labels, so board-poll drift is assignee/label-blind; only REST `fetch_remote` reads them.

## 2. Decisions

### D1 — One canonical mirrored-field set as the single source of truth

Define a canonical mirrored-field set in `domain-task` (a `MIRRORED_FIELDS` constant or a typed `MirrorField` enum). Detection, the field-level diff, the DTO mapping, and `remote_mirrors_baseline` all reference it, so the three definitions can never drift again. **In scope: `{title, body, status, assignees}`** — the existing detection set. Labels: an explicit in/out decision (see D8; recommend defer). Milestone: out of scope (non-goal).

### D2 — `Task::diff_against_baseline` produces a `MirrorPatch`

Add `Task::diff_against_baseline(&self) -> MirrorPatch`, where:

```text
MirrorPatch {
    title:     Option<String>,
    body:      Option<String>,
    status:    Option<TaskStatus>,
    assignees: Option<Vec<String>>,
}
```

Each field is `Some` **iff** it differs from `synced_baseline`, reusing the *same* comparators as detection — string `!=` for title/body, and `assignees_equal` for the unordered assignee set (so a pure reorder never produces a push). Extract the per-field comparison currently inlined in `reconcile_dirty_against_baseline` into shared predicates so detection and patch-building cannot diverge. With no baseline (local-only task), the diff is empty — the push paths already special-case "no remote."

> Note: `TaskSnapshot`'s derived `PartialEq` includes `version` and `captured_at`, which the repository overwrites at write time. The diff helper must therefore compare field-by-field (as `reconcile_dirty_against_baseline` does) and never via whole-snapshot equality.

### D3 — Field-complete port DTO with `Option` set-semantics

Widen `ports::RemoteTaskUpdate` with `assignees: Option<&[String]>`:

- `None` → leave the remote's assignees unchanged (don't send the field).
- `Some([…])` → set the assignee set to exactly this list.
- `Some([])` → **clear** all assignees.

This matches the `Option` set-semantics title/body already use and restores create/update symmetry (`RemoteTaskCreate` and `RemoteTaskSnapshot` already carry assignees). Labels are widened symmetrically only if D8 puts them in scope.

### D4 — Re-derive the diff at drain time; keep the outbox payload coarse

Both `SyncService::push` and `OutboxDrainer::apply` call **one shared helper** (analogous to `lifecycle_to_remote_state`) that builds a `RemoteTaskUpdate` from the **live task diffed against its `synced_baseline`**. The on-disk `OutboxMutation::UpdateRemote` payload stays coarse and continues to be ignored at drain time (exactly as title/body/closed are today).

Rationale: the drainer already re-reads the live task and re-derives `(closed, state_reason)`, so this is the established pattern. It preserves two properties for free:

- **Edit coalescing** — multiple edits to one `DirtyLocal` task collapse into the next drain; no per-edit outbox row.
- **Idempotency by full overwrite** — replaying a `RemoteTaskUpdate` re-sends the same derived set; at-least-once delivery stays safe.

It also needs **no outbox serde migration**. (Alternative — persisting the computed patch into the outbox row — is rejected in §4.)

### D5 — Re-baseline only the fields actually transmitted

Add `Task::confirm_synced_fields(source, &MirrorPatch)` (or thread the transmitted patch through `confirm_synced`). The baseline advances per-field **only** for fields actually pushed. This closes the silent-loss class for *any* field whose channel is incomplete: a field that wasn't transmitted stays dirty and gets retried, instead of being hidden by a premature rebaseline.

Two hard rules:

- **Never re-baseline from the PATCH response.** `update_issue` returns the mapped PATCH response, which reflects `assignees=[]` whenever the PATCH didn't set them. Re-baselining from it would overwrite local assignee intent with GitHub's empty set — compounding the bug. Re-baseline from the transmitted patch / live task.
- **The daemon drainer must re-baseline on `UpdateRemote` success.** Today the drainer marks the outbox entry succeeded but does **not** call `confirm_synced` / `set_synced_baseline`. If it starts pushing assignees without advancing the baseline, the task looks perpetually dirty. The drainer's `UpdateRemote` arm must `confirm_synced_fields` + save on success with `SnapshotSource::Push`.

### D6 — REST adapter sets assignees on update

`RestClient::update_issue` calls `.assignees(...)` when the DTO field is `Some`, mapping `Some([])` to an explicit clear. octocrab's `UpdateIssueBuilder` serializes title, body, state, state_reason, **and assignees** (and labels, and milestone) in a single PATCH, so a field-complete push is **one request** — no extra round-trip and no add/remove-delta endpoint. (Labels symmetrically only if D8 includes them.)

### D7 — Per-backing-kind field sets; status stays an outbound-only projection

- **Draft-backed mirrors** (`OutboxMutation::UpdateDraftIssue`) carry an **assignee-less** field set — GitHub Projects v2 drafts have no assignee concept. The shared helper must vary by backing kind (issue-backed vs draft-backed).
- **Status** remains a one-way lossy projection via `lifecycle_to_remote_state`, with its own diff rule; pull continues **not** to copy open/closed back onto the 5-state lifecycle. Status is *not* folded into the symmetric field-level diff (see the pull/push asymmetry risk in §5).

### D8 — Labels: explicit scope decision (recommend defer)

Labels touch all eight layers (a `Task` field, a `TaskSnapshot` column + migration, detection, both DTOs, both push paths, the adapter, and pull copy-back). Recommendation: **defer** labels to a follow-up and document them as a non-goal of this RFC, keeping the diff scoped to the confirmed bug. If included, labels enter D1's canonical set and ride D2–D6 symmetrically with assignees. Milestone is out of scope regardless.

## 3. Non-goals

- Milestone mirroring.
- Pulling remote open/closed back onto the 5-state lifecycle (status stays outbound-only).
- Comments and relations — they have their own drain channels and are outside the snapshot diff.
- The `list_changed_since` REST polling fallback (GitHub inbound change-detection runs through the Projects v2 GraphQL poll).
- **Label mirroring (D8, decided 2026-06-08).** Labels are workflow vocabulary owned by the user's GitHub org/repo automations, not state repo-link can authoritatively mirror. See D8.
- Activating a field-level conflict model (see §6).

## 4. Alternatives considered

- **Persist the computed patch into `OutboxMutation::UpdateRemote` at enqueue time.** The row would authoritatively record exactly what changed (nice for dead-letter inspection). Rejected: it's a persistence-format change (the enum is serde-tagged in lockstep with the `mutation_kind` column; new fields need `#[serde(default)]` for legacy rows), and it *breaks* coalescing unless the drainer still re-diffs against the baseline anyway — defeating the point. Two successive assignee edits would need accumulate-or-overwrite semantics the live re-derive (D4) gets for free.
- **Widen `RemoteTaskUpdate` + adapter for assignees only, no diff machinery.** Smallest change; directly fixes xms. Rejected as the *primary* design: it leaves the blanket-send model intact, keeps the three divergent field-set definitions, and doesn't fix the silent-rebaseline class for the next incomplete field. (It is, however, a viable first slice — see the ticket order.)
- **Make pull reconcile remote open/closed back onto local status.** Would make status symmetric, but GitHub's two states have no faithful inverse into the 5-state lifecycle (Open/InProgress/Blocked all map to open). Keep status outbound-only.

## 5. Risks

- **Silent-rebaseline (the core bug class).** Any push that advances `synced_baseline` to the live task while transmitting only a subset of fields permanently hides the dropped field. D5 (re-baseline only transmitted fields) is the mitigation and is load-bearing.
- **Rebaseline-from-PATCH-response.** `update_issue` returns `assignees=[]` when the PATCH didn't set them; re-baselining from the response would clobber local intent. D5 forbids it.
- **Two construction sites, two apply sites.** The payload is built in both `SyncService::push` and `enqueue::plan_mutations`, and applied in both `push` and the drainer. Fixing one leaves the other broken — D4's single shared helper is the guard. The daemon path additionally does not re-baseline on `UpdateRemote` success today (D5).
- **Pull/push asymmetry on status.** Pull re-baselines via `snapshot_view`, which records the *unchanged local* status, not the remote's. A unified diff that included status could record stale local status into the post-pull baseline and mis-diff on the next push. D7 keeps status outbound-only with its own rule.
- **Whole-set assignee replacement.** GitHub PATCH assignees is PUT-like (whole-set). `assignees_equal` already treats assignees as an unordered set, so replacement is semantically correct — but it can clobber an assignee added directly on GitHub between syncs. Whether the pull-before-push ordering / conflict policy adequately guards this is an open question (§6).
- **`Some([])` vs `None` ambiguity.** The DTO and the octocrab mapping must distinguish clear-all from leave-unchanged; an empty vec must map to an explicit clear, not a no-op.
- **Migration.** D4 needs no data migration. The duplicated baseline-eligibility predicate (Rust `is_baseline` vs the SQL `WHERE`) must be unified or its coupling documented — any change to mirrored fields or `SnapshotSource` otherwise risks the loaded baseline silently disagreeing with the domain. The tasks-table full-row upsert means a field-level write can only live in the outbox payload / port DTO, never the task-row write.

## 6. Open questions

1. **Labels in or out of D1 scope** (D8). Recommendation is defer; the RFC owner decides.
2. **`Some([])` semantics against octocrab / GitHub** — confirm an empty assignees array maps to an explicit clear, not an ignored field.
3. **Daemon re-baseline + concurrent pull.** Today only push re-baselines. Having the drainer re-baseline on `UpdateRemote` success (D5) interacts with the `SyncPolicy::ManualMerge` conflict path; the conflict-window was not exercised.
4. **Direct-on-GitHub assignee edits.** Whether whole-set replacement can clobber them between syncs, and whether `decide()`'s ordering protects against it.
5. **`ConflictKind::AssigneeMismatch`** exists in `domain-sync` but is currently unused by `decide()` (which routes on `SyncState` + a precomputed bool, not per-field). Whether to activate a field-level conflict model is open and likely out of scope here.

## 7. Testing strategy

There is no failing test guarding assignee propagation today — the harness structurally cannot observe it. Both recording stubs (`testing-fixtures`'s `InMemoryRemoteTaskProvider` and the in-file `FakeProvider` in `application-sync`) lack an `assignees` field on their recorded-update structs, and the `infra-github` `update_issue_*` tests build `RemoteTaskUpdate` without assignees and assert only state/state_reason.

Prerequisite test-infra change: add `assignees` to both recorded-update structs and record `cmd.assignees` in both `update_remote` stubs. New tests:

- `SyncService::push` sends a changed assignee through `update_remote`.
- The drainer re-derives assignees from the **live task** (mirror of the existing "pushes live task title/body not payload snapshot" test).
- The `infra-github` wiremock PATCH body includes the assignees array.
- An assignee **reorder** does **not** push.
- `Some([])` clears assignees.
- Re-baseline-only-transmitted-fields regression: an incomplete channel leaves the field dirty rather than hiding it.

Widening `RemoteTaskUpdate` will break the two existing `update_issue_*` tests by struct literal — the intended tripwire.

## Appendix A — current field matrix

| Field | on `Task` | on `TaskSnapshot` | in reconcile diff | in `RemoteTaskCreate` | in `RemoteTaskUpdate` | in `OutboxMutation::UpdateRemote` | written by REST update | pulled back |
|---|---|---|---|---|---|---|---|---|
| title | yes | yes | yes | yes | yes | yes (ignored; re-derived live) | yes | yes |
| body | yes | yes | yes | yes | yes | yes (ignored; re-derived live) | yes | yes |
| status (open/closed) | yes | yes | yes | no | partial (`closed`+`state_reason`, 5→2) | partial (`closed`; re-derived live) | partial (state_reason only when closing) | no (deliberate) |
| **assignees** | yes | yes | yes | yes | **no** | **no** | **no** | yes |
| labels | no | no | no | yes (always `&[]`) | no | no | no | no (fetched, dropped) |
| milestone | no | no | no | no | no | no | no | no |

The detected-set minus the propagated-set is exactly **{assignees}**.
