# RFC 0006 — Priority and Type field propagation

Status: Draft (2026-07-17; **amended 2026-07-20** post-implementation — see §0)
Tracking epic: **#222**
Amends:
- RFC 0001 §6 "Port and schema sketches" (the `projects` schema, the `RemoteProjectSnapshot` shape, and the `set_status` mutation, which becomes the general `set_single_select_option` setter).
- ~~RFC 0003 §D1 "canonical mirrored-field set" (adds native issue **type** to the issue-side mirror set) and §D2–D6 (type rides the same diff/patch/re-baseline rail as assignees).~~ **Retracted (§0, A1):** implementation proved type CANNOT ride the issue mirror — it is a separate GraphQL `updateIssue` projection, so `MIRRORED_FIELDS` is left unchanged and RFC 0003 is not amended after all.
- The `Task::set_priority` "priority is local-only metadata, never synced" decision in `domain-task` — this RFC flips priority to a synced, outbound projection.

## 0. Amendments (2026-07-20, post-implementation)

The §2 decisions below are preserved as originally written; the corrections learned while building #223–#228 are recorded here and cross-referenced inline. Where this section conflicts with a §2 decision, **this section wins**.

### A1 — Type is a GraphQL `updateIssue` projection, NOT an issue-mirror field (supersedes D6, D1's Type bullet, the §1 table's Type row, and Appendix A's Type rail)

D6 assumed type rides the issue's REST PATCH inside RFC 0003's `MIRRORED_FIELDS`. That is **impossible**: octocrab 0.51's issue builder (`RestClient::update_issue`) has no `issue_type` slot, and GitHub only sets a native type via GraphQL `updateIssue(input: { id, issueTypeId })`. Joining `MIRRORED_FIELDS` would also be self-defeating (dirty-detection is computed *over* that set, and it would route type into the REST patch that can't express it — the no-op-REST trap).

So type is modeled **exactly like Priority (A2)**: a dedicated `OutboxMutation::SetIssueType { issue_node_id, issue_type_id: Option<String> }`, its own drainer arm (Ok→Stamped / Err→Retry, **no read-back Conflict**), **off** the issue sync-axis. `set_issue_type` does not enter `MIRRORED_FIELDS`/`MirrorPatch`; `TaskSnapshot` carries it only for local history and rollback, guarded by `issue_type_recorded` so legacy snapshots preserve the live value. It is never a dirty-detection or projection baseline. The enqueued outbox entry remains the durable remote retry unit. Resolution: local `IssueType` → canonical name → `issue_type_id` via the org registry (**case-insensitive** — local Display is lowercase, registry stores capitalized), at the app layer (`TaskService`), owner taken from the filing repo. `issue_type_id: None` clears the type. This resolves **§6 Q4** (GraphQL, not REST) and **§6 Q5** (`null` clears).

### A2 — Priority uses a dedicated `SetProjectPriority` outbox variant (resolves §6 Q1)

Not a reuse of `SetProjectStatus`. Its drainer arm Stamps on any `Ok` (a value mismatch never Conflicts — a priority board disagreement must not flip the issue-axis `sync_state`) and Retries on transient error. `TaskService::update` plans it on a dedicated branch (not via `plan_update_mutations`) so a priority-only edit never emits a no-op issue `UpdateRemote`.

### A3 — No structured `SyncNoticeDto` variants; advisories are plain `tracing::warn!`/`debug!` (supersedes D10)

D10 proposed new `SyncNoticeDto` variants. **Dropped.** The advisory cases (`PriorityClamped`/`PriorityFieldMissing`/`PriorityUnmapped`; type `Unavailable`/`Unmapped`/dropped) all occur on the async drainer or on off-axis projections that never return a `SyncSummaryDto`, so there is no synchronous surface to render a structured notice into. They are surfaced with plain `tracing::warn!` (misconfiguration) / `tracing::debug!` (expected-transient, e.g. not-yet-attached), consistent across #224/#225/#226/#228. Reintroduce a structured variant only if a synchronous CLI surface later needs it.

### A4 — Parse is infallible; relation-aware defaulting is a separate follow-up (supersedes D7's "validated at set time / rejected", resolves §6 Q3)

`IssueType::from` is infallible (unknown → `Custom`, never rejected). The **relation-aware default policy** — a sub-issue (`child_of`) defaults to the org's sub-issue type (e.g. Task/Subtask), a free-standing issue to the standalone default (e.g. Story), always overridable — was **not** in this RFC's scope (D7 only modelled the enum + explicit set). It is split to a follow-up task (`rpl-qa5`, blocked on #228). Its defaults are org-specific, dynamic names and must be **validated against the live `org_issue_types` registry** (A1's cache), not hardcoded; config location (workspace setting vs mapping) is that task's open question.

### A6 — Custom "Type"/"Types" project field IS now supported (supersedes the §3 non-goal), preferred over native issue Type (#238)

§3 listed the custom **"Types"** Projects v2 single-select as a non-goal ("if ever wanted it rides the Priority rail unchanged"). #238 implements it — and it does **not** ride the Priority rail unchanged, because Priority maps by *ordinal* (D3) whereas a custom Type field maps by **case-insensitive option name** (an open, unordered vocabulary, exactly as native Type resolves by name in A1). The decisions:

- **Two rails, one per task, custom preferred.** A board with an **unambiguous** custom single-select named `Type` or `Types` (case-insensitive) takes a dedicated project-item `OutboxMutation::SetProjectType`; otherwise the native `SetIssueType` rail (A1) is the fallback. Never both for one task — the choice is centralized in `application-sync::enqueue::resolve_type_projection`, which **every** issue-type-projection trigger routes through (edit, rollback, the `issue_type_pending` sweep, `promote`, orphan-draft convert, and the first-attach follow-up). This is why the custom rail is dogfoodable on repo-link's own **user-owned** test board #3, which has no native issue types (D8).
- **Ambiguity is not a hard error.** More than one `Type`/`Types` single-select → warn and skip the custom projection (native fallback), rather than choosing arbitrarily or failing the board link (contrast the >1-Priority hard error, which is safe because Priority is auto-classified from the literal name "Priority"). `Project::type_field` returns `Some` only when exactly one is classified; `has_ambiguous_type_field` drives the warn.
- **Clear support.** The project-item single-select rail gained a clear path: the generalized `RemoteProjectProvider::set_single_select_option` now takes `Option<&str>` (`None` → GraphQL `clearProjectV2ItemFieldValue`), so an explicit `--clear-type` removes the custom field value. When the custom rail is unavailable the clear falls to native Type.
- **Persistence.** A new `ProjectFieldKind::Type` is persisted (additive migration `20260723000002` widens the `project_fields.kind` CHECK to include `'type'` via a stash/rebuild, since SQLite can't relax a CHECK in place). There is **no** Type mapping table — resolution is by option name against the retained `project_field_options` catalog, so no `project_type_mappings` analog to `project_priority_mappings` exists.
- **Shared durable intent.** `SetProjectType` carries the same `local_issue_type` / `local_issue_type_recorded` compare-and-clear fields as `SetIssueType` and clears the SAME `issue_type_pending` flag on success, so whichever rail applies retires the intent and the daemon sweep is rail-agnostic. The outbox dedupe (`insert_outbox_in_tx` + `idx_outbox_set_issue_type_dedupe`) was generalized to cover both `set_issue_type` and `set_project_type`.

### A5 — Delivery

Sliced into six tasks under epic #222: #223 (generalized field model, D2/D9), #224 (priority mapping, D3), #225 (priority projection, D4/A2), #226 (org issue-type registry, D5), #227 (local `IssueType` enum, D7), #228 (type-on-issue mechanism, A1). #223/#224/#225/#226/#227 are merged; #228 is in progress; defaulting (`rpl-qa5`) is a follow-up. Recommended order held: Priority track before Type.

## 1. Context

repo-link mirrors a local `Task` to a GitHub Issue (RFC 0003 issue-side mirror) and projects a board **Status** option onto the task's Projects v2 item (RFC 0001 / RFC 0004). Users now want two more sidebar fields driven automatically: **Priority** (shown as `Medium`) and **Type** (shown as `Task`).

These two fields look similar in the GitHub UI but come from **two different APIs at two different scopes**, and that asymmetry is the organizing fact of this RFC:

| Sidebar field | What it actually is | Scope | Set via |
|---|---|---|---|
| Priority (`Medium`) | Projects v2 single-select **custom field** | per **project** | `updateProjectV2ItemFieldValue` (the mutation `set_status` already uses) |
| Type (`Task`) | GitHub **native issue type** | per **organization** | REST `PATCH /issues/{n}` `"type": "<name>"` (or GraphQL `updateIssue(issueTypeId:)`) |
| "Types" (empty, under Fields) | a *separate* Projects v2 single-select custom field | per project | `updateProjectV2ItemFieldValue` |

Note the third row: the sidebar also shows an empty custom **"Types"** project field, distinct from the native **"Type: Task"** at the top. This RFC originally targeted only the **native issue type** and listed the custom field as a non-goal; **§0 A6 (#238) supersedes that** — the custom "Type"/"Types" field is now supported and *preferred* over native Type, resolving by option name (not the Priority ordinal rail).

### What exists today

- **`GraphqlClient::fetch_project`** (`crates/infra-github/src/graphql.rs`) already queries `fields(first: 50)` and pulls **every** single-select field with its options over the wire — but then **collapses the whole set to a single field named `Status`** (else the first single-select) and discards the rest. `RemoteProjectSnapshot` carries only `status_field_id` + `status_options`; there is no per-field identity.
- **`GraphqlClient::set_status`** is already the **generic** `updateProjectV2ItemFieldValue(fieldId, singleSelectOptionId)` mutation. It is field-agnostic on the wire; only the Rust method name and the single threaded `status_field_id` make it Status-specific.
- **The domain is Status-specific.** `Project` (`crates/domain-project/src/project.rs`) has exactly one field slot: `status_field_id`, `status_options: Vec<StatusOption>`, `status_mappings: Vec<StatusMapping>`. `StatusOption { option_id, name, ordinal }` is generic enough for any single-select option; `StatusMapping { is_open, option_id }` is lifecycle-specific and does **not** generalize.
- **Status-field derivation** happens in two places: which single-select becomes "Status" is decided in `fetch_project`; the option→lifecycle mapping is `derive_status_mappings` (`crates/domain-project/src/mapping.rs`), run at link time by `ProjectService::link_from_snapshot`.
- **DB schema** (`crates/infra-sqlite/migrations/20260528000003_project_sync.sql` + the `_project_status_mappings` migrations): `projects.status_field_id` is the only field id anywhere; `project_status_options` and `project_status_mappings` are implicitly Status-only (no field-identity column).
- **Priority already exists locally but is never synced.** `domain-task/src/enums.rs` defines `Priority { P0, P1, P2, P3 }`; `Task::set_priority` is explicitly "local-only metadata: GitHub doesn't model it, so a priority change does NOT flip sync state." It's persisted in `tasks.priority` and surfaced read-only in query DTOs. It appears in no wire write.
- **Native issue type is absent everywhere.** No `IssueType` / issue `type` reference. `RestClient::create_issue` / `update_issue` set only title/body/assignees/labels/state; `RemoteTaskCreate` / `RemoteTaskUpdate` (`crates/ports/src/remote_task.rs`) carry `labels` but no `type` or `priority`.
- **An advisory channel already exists.** `SyncSummaryDto` (`crates/dto-shared/src/sync.rs`) carries `messages: Vec<SyncNoticeDto>` — a structured, internally-tagged notice enum (modeled on `DomainEvent`, prose formatted by the CLI) — plus a free-text `note`. Notices are built via `summary_with_messages` (`application-sync::summary`), produced today only by `SyncService::pull` (inbound relation reconcile, PR #217 / #150), and rendered to stderr by `app-cli::render::sync` (`sync_notice_line` is the single prose source; stdout stays scriptable JSON). This is the reuse target for D10.

### The gap

- Priority: the wire plumbing exists (fetched-then-discarded field; generic mutation); the domain, schema, and sync trigger do not model a second project field.
- Type: absent at every layer, on a different API and scope than everything else the system touches.

## 2. Decisions

### D1 — Two rails, by API, not one symmetric "field" abstraction

Priority and Type are propagated on **different rails** because they live on different GitHub objects:

- **Priority → the project-item rail.** It is a Projects v2 single-select on the *project item*, resolved to an opaque per-project `option_id`, and set with the same `updateProjectV2ItemFieldValue` mutation that projects Status onto the board. It does **not** touch the issue PATCH path.
- **Type → the issue rail (RFC 0003).** The native issue type lives on the *issue*, is set by **name** via `RestClient::update_issue`, and joins the RFC 0003 mirrored-field set alongside title/body/assignees.

We deliberately do **not** unify these into one generic "remote field" abstraction. They share the mapping-persistence idea (§D2 storage shape) but nothing at the propagation layer.

### D2 — Two project-field mappings per-project; issue type keyed by org

The two Projects v2 fields (Status, Priority) hang off `project_id` and resolve the same way. The native issue **type** is not a project field — it is org-scoped, so it is keyed by `owner_login` (D5), decoupled from any board:

```
project_status_mappings    (project_id, is_open)      → option_id                          # exists
project_priority_mappings  (project_id, priority)     → option_id                          # new, D3
org_issue_types            (owner_login, local_type)  → issue_type_name (+ issue_type_id)  # new, D5
```

To hold the second project field's identity + options without a third copy of the Status-specific schema, **generalize the project field model**:

- Add `project_fields (project_id, field_id, name, kind)` and `project_field_options (project_id, field_id, option_id, name, ordinal)`, and migrate the existing Status field/options into them. `project_status_mappings` and `project_priority_mappings` both FK into `project_field_options`.
- `Project` grows from a single `status_field_id` slot to a small keyed set of fields (`status`, `priority`), each with its own `field_id` + options. `StatusOption` becomes a generic `FieldOption`. `StatusMapping` (lifecycle-specific) stays; `PriorityMapping` is its sibling.

(A minimal alternative — bare `projects.priority_field_id` column + a `field_id` discriminator bolted onto `project_status_options` — is viable as a first slice but reproduces the Status special-casing a third time; see §4.)

### D3 — Priority: reuse the local `P0..P3` enum, map by **ordinal** ("incremental")

The local source stays the existing `Priority { P0, P1, P2, P3 }` enum. It maps onto each board's option set by **ordinal position**, never by name, so it lands correctly on any board regardless of what the options are called (`High/Med/Low`, `Urgent/High/Med/Low`, `P0..P3`, …):

```
board options (ordinal order)     P0  P1  P2  P3
4 opts (Urgent/High/Med/Low)      0   1   2   3      exact
3 opts (High/Med/Low)             0   1   2   2      clamp tail
5 opts                            0   1   2   3      trailing option unused
```

- **Count mismatch** (P0..P3 is four buckets; boards have N): default to clamp — `Pk → option[min(k, N-1)]`, mirroring how `derive_status_mappings` falls back to first/last.
- Derived at **link time** by a new `derive_priority_mappings(&[FieldOption])` sibling of `derive_status_mappings`, stored in `project_priority_mappings`, and **overridable** by hand (the derivation is a cache, not the source of truth).
- **Which single-select field is "Priority"** is chosen the same way Status is: match a field literally named `Priority`, else leave unmapped (Priority sync is opt-in per project — absence of a Priority field is not an error).

### D4 — Priority is an outbound project-item projection (amends `set_priority` local-only)

> **Refined — see §0 A2.** The open "which outbox mutation" question (§6 Q1) resolved to a dedicated `SetProjectPriority` variant (no read-back Conflict), planned off `plan_update_mutations`.

- Generalize `GraphqlClient::set_status` into a field-agnostic `set_single_select_option(project_id, item_id, field_id, option_id)`; `set_status` becomes a caller with the Status field id, and Priority a caller with the Priority field id + `resolved` option from `project_priority_mappings`.
- `Task::set_priority` **flips from local-only to a synced projection**: a priority change now marks the task's board projection dirty, the same way a status change drives the board Status option. Priority rides the *project-item* dirty/outbox path, **not** the issue PATCH / `MirrorPatch` diff (D6).
- Like Status (RFC 0003 §D7), Priority is an **outbound-only projection**: pulling remote priority back onto the local `P0..P3` is a non-goal (§3). The exact outbox representation reuses whatever mechanism sets the board Status option today; confirming that wiring is an implementation task (§6).

### D5 — Type: registry keyed by org, decoupled from any board

`org_issue_types (owner_login, local_type) → issue_type_name (+ issue_type_id)`:

- **Keyed by org, not project.** Native issue types live on the issue and are defined once per org, shared across that org's repos — independent of whether the task is on a board. Keying by `owner_login` means Type resolves for **any** synced issue, including workspaces with no project (`workspace.project_id` is nullable, RFC 0001 D1). This is the one field that deliberately does **not** ride the `project_id` axis (Status and Priority still do). A board that spans multiple orgs is handled for free: each org has its own rows.
- **The rows are a cache of the org's issue-type registry** (fetched from `organization.issueTypes`), re-derived when the org is (re)fetched — not the source of truth.
- **Resolution at push time:** `task → filing repo → owner org → (owner_login, local_type) → issue_type_name`. The task's org comes from its **filing repo** (the logical-repo-vs-filing-repo split, RFC 0002, exists precisely so we know where the issue is filed). No project needed.
- **Disambiguator:** `owner_login` (consistent with `projects.owner_login`); carry the org **node id** alongside only if org-rename stability bites (§6).

### D6 — Type joins the RFC 0003 issue mirror set, set by name via REST

> **⚠ Superseded — see §0 A1.** This decision is wrong: octocrab can't carry `type` in the REST PATCH, so type is a dedicated GraphQL `updateIssue` projection (a `SetIssueType` outbox mutation), **not** a `MIRRORED_FIELDS` member. Text retained for history.

- Add native `type` to RFC 0003's canonical `MIRRORED_FIELDS` and to `MirrorPatch`, `RemoteTaskCreate`, `RemoteTaskUpdate`, with the same `Option` set-semantics (`None` = leave unchanged, `Some(name)` = set, `Some(clear)` = remove).
- `RestClient::create_issue` / `update_issue` set the type **by name** (`"type": "<issue_type_name>"`) — octocrab serializes it in the same PATCH as title/body/assignees, so it stays **one request**. GraphQL `updateIssue(issueTypeId:)` is the alternative if the REST `type` name proves unreliable; the mapping stores both name and id to keep that option open (§6).
- Re-baseline only the transmitted `type` (RFC 0003 §D5), so a silently-dropped type (D8) stays dirty and retries rather than being hidden.

### D7 — A local `IssueType` enum on `Task`: well-known variants + `Custom` passthrough

> **Refined — see §0 A4.** Parse is infallible (unknown → `Custom`, not rejected); the relation-aware *defaulting* policy is split to follow-up `rpl-qa5`.

Issue types are an open, org-configurable set — a *closed* enum would be too rigid, but a plain string gives no ergonomics for the common case. Model it as an **extensible enum** on the task:

```text
enum IssueType {
    Task,           // GitHub built-in defaults
    Bug,
    Feature,
    Custom(String), // org-specific types (Epic, Story, Chore, …)
}
```

`Task.issue_type: Option<IssueType>` is the local source that D5 maps to a concrete org type name. The well-known variants cover GitHub's built-in defaults; `Custom(String)` carries anything an org defines beyond them. Values are validated against the org's fetched registry at set time (unknown → rejected or warned, TBD §6). Unlike Priority's `P0..P3` (a *closed, ordered* set mapped by ordinal, D3), `IssueType` is *open and unordered*, which is exactly why it resolves through D5's explicit name map rather than by position.

### D8 — Type availability + reliability handling

Native issue types have two hard constraints the design must handle gracefully, not assume away:

- **Org-only.** Issue types do not exist on user-owned repos/boards. On such repos, Type sync is simply unavailable — detect an empty/absent org issue-type registry at link time and disable the Type rail for that project (no error). (Note: repo-link's own test board #3 is user-owned, so the Type path cannot be exercised there — it needs an org repo to test.)
- **Silently dropped without push access.** GitHub drops the `type` write with no error if the token lacks push access. Detect availability up front (successful org issue-type fetch) and surface a warning; combined with D6's re-baseline-only-transmitted rule, a dropped type stays dirty rather than being falsely recorded as synced.

### D9 — `fetch_project` stops discarding non-Status single-selects

`fetch_project` retains all single-select fields (it already fetches them) so the Priority field's id + options survive into `RemoteProjectSnapshot` and `project_field_options`. Field selection (which is Status, which is Priority) moves out of the adapter into named matching over the retained set.

### D10 — Surface unmappable / unavailable field cases as `SyncNoticeDto`, not errors

> **⚠ Superseded — see §0 A3.** No structured `SyncNoticeDto` variants were built; advisories are plain `tracing::warn!`/`debug!` (the drainer/off-axis paths have no synchronous summary to render into). Text retained for history.

Both fields have advisory cases that must **not** hard-fail a sync — reuse the existing notice channel (§1) with new variants rather than a parallel mechanism. New `SyncNoticeDto` variants:

- `PriorityClamped { task_id, priority, option_name }` — the local priority collapsed onto a shared board option because the board has fewer options than `P0..P3` (D3 clamp; two priorities → one option). (Note: a literal "P4" cannot occur — `Priority` is a closed `P0..P3` enum — so the real analog of the user's "P4" example is clamp/collapse against a smaller board.)
- `PriorityFieldMissing { project_id }` — a priority is set locally but the board has no Priority single-select to project onto (D3 is opt-in).
- `TypeUnavailable { owner_login }` — the filing repo's org has no issue types (user-owned repo or feature disabled, D8); the type write is skipped.
- `TypeDropped { task_id, issue_type }` — GitHub silently dropped the type write for lack of push access (D8); with D6's re-baseline-only-transmitted rule the task stays dirty and retries.
- `TypeUnmapped { owner_login, local_type }` — the local `IssueType` (typically a `Custom(_)`) matched no name in the org registry (D5 / D7).

Two constraints inherited from the existing abstraction:

- **`messages` is `pull`-only today.** Priority/Type are outbound (link / promote / push), so this **widens which verbs emit `messages`** — a deliberate extension. `summary_with_messages` already exists to build the summary for any verb; each new variant needs a `sync_notice_line` prose arm.
- **The daemon drainer has no interactive summary.** A drop/unavailable case that occurs during async `OutboxDrainer` drain has no `SyncSummaryDto` to render into, so it falls back to daemon logging. The notice channel covers the **synchronous** verbs (`set`-time validation, `link`-time derivation, `promote`/`push`); daemon-time occurrences are logged. Whether to persist notices for later surfacing is an open question (§6).

## 3. Non-goals

- ~~The empty custom **"Types"** Projects v2 field (distinct from native issue type). If ever wanted it rides the Priority rail (D3/D4) unchanged; out of scope here.~~ **Superseded — see §0 A6 (#238):** the custom "Type"/"Types" field IS now supported, resolves by case-insensitive option NAME (not the Priority ordinal rail), and is preferred over the native issue-type rail.
- Other sidebar fields: **Effort**, **Due date**, **Labels** (RFC 0003 §D8 defers labels), **Milestone**.
- **Inbound pull** of priority or type back onto the local task — both are outbound-only projections initially, like Status (RFC 0003 §D7 / RFC 0004).
- A per-field **conflict model** for priority/type (parallels RFC 0003's dormant `AssigneeMismatch`).
- Cross-org **type vocabulary reconciliation** beyond the org-keyed disambiguation in D5.

## 4. Alternatives considered

- **Per-project denormalized type table** `project_org_type_mappings (project_id, org, local_type)`. Keeps Type on the same uniform `project_id` axis as Status/Priority. **Rejected** (D5): native issue type is decoupled from boards, so keying by project breaks for workspaces with no project (`project_id` nullable) and stores an org's type list redundantly across its projects. Org-keying (D5) is both correct-scope and simpler, at the cost of Type no longer sharing the `project_id` axis.
- **Name-based priority mapping** (match `High`/`Medium`/`Low` like `derive_status_mappings` matches status vocab). **Rejected**: priority vocabularies vary too much across boards (`P0..P3` vs `Urgent/High/Med/Low` vs `High/Med/Low`); ordinal (D3) maps correctly without a vocabulary list. Name hints could later *refine* the ordinal default.
- **Bare per-field columns** (`projects.priority_field_id` + a `field_id` discriminator on `project_status_options`) instead of the generalized `project_fields` model (D2). Viable first slice, smaller migration; **not** the primary design because it reproduces the Status special-casing a third time and blocks the custom "Types" field cleanly reusing the rail.
- **Set type via GraphQL `updateIssue(issueTypeId:)` only.** Avoids the REST name lookup but adds a second write path (issue fields otherwise go through the REST PATCH). **Deferred** to a fallback; D6 stores both name and id so it stays available.

## 5. Risks

- **Ordinal ordering assumption (D3).** Ordinal mapping assumes the board lists priority options in severity order. A board that orders options arbitrarily would mis-map. Mitigation: derivation is overridable, and Priority sync is opt-in per project.
- **Count mismatch (D3).** P0..P3 vs N options: clamp is a lossy default (two local priorities can collapse to one option). Documented and overridable.
- **Flipping priority to synced (D4)** reverses an explicit invariant and interacts with dirty detection. Priority must ride the *project-item* projection path, not RFC 0003's `MirrorPatch` (which is issue-side); conflating them would double-count or mis-baseline. The board-status projection is the template to follow.
- **Silent type drop (D8).** Without push access GitHub drops `type` with no error. Re-baseline-only-transmitted (D6) is the load-bearing guard; without it a dropped type is silently recorded as synced.
- **Org-only type (D8).** User-owned boards have no issue types; the test board is user-owned, so Type needs an org repo to exercise — a testing-infra gap, not just a runtime branch.
- **Registry staleness (D5).** Org type renames/additions leave the `org_issue_types` cache stale until the org is re-fetched. Acceptable because the rows are a cache with a defined refresh point (org (re)fetch), not the source of truth.
- **Org resolution (D5).** Type resolution depends on correctly deriving a task's org from its filing repo. If the filing-repo→owner mapping is wrong, type resolves against the wrong org's vocabulary.
- **Migration.** The generalized `project_fields` / `project_field_options` model (D2) requires migrating existing Status rows. Per the project's sqlx-sqlite constraint, prefer additive tables + backfill over parent-table rebuilds; do not edit shipped migrations in place.

## 6. Open questions

1. **Exact outbox representation for the priority projection (D4).** Reuse the board-status projection's outbox mutation, or a dedicated `SetProjectItemField` variant? Depends on how Status projection is currently enqueued — confirm during implementation.
2. **Priority pull-back.** Kept outbound-only here; is one-way sufficient long-term, or will drift on the board need reconciling (parallels the Status open/closed asymmetry)?
3. **Local `Task.issue_type` validation (D7).** Reject unknown type names hard, or warn-and-store? And what is the value at task creation — unset by default, or a workspace/repo default?
4. **REST `type` by name vs GraphQL `issueTypeId` (D6).** Confirm the REST `"type"` name write is reliable; if not, switch to `updateIssue(issueTypeId:)`. Mapping stores both to keep the choice open.
5. **`Some([])`/clear semantics for type** — confirm `"type": null` clears the issue type via octocrab's PATCH mapping (parallels RFC 0003 §6 Q2 for assignees).
6. **Org disambiguator (D5)** — `owner_login` (readable, rename-fragile) vs org node id (stable, opaque). Start with `owner_login`; carry node id only if renames bite.
7. **Slicing.** Priority (project-item rail, no new API, testable on the user-owned board) and Type (issue rail, org-only, needs an org repo) are independently shippable. Recommend Priority first.
8. **Daemon-time notices (D10).** Drops/unavailable cases during async `OutboxDrainer` drain have no `SyncSummaryDto` to render into. Log-only for now, or persist notices (e.g. on the outbox entry / a notices table) so a later `rl` command can surface them? Log-only is the smaller first step.

## 7. Testing strategy

**Priority (project-item rail):**
- `derive_priority_mappings` ordinal derivation: 4-option exact, 3-option clamp-tail, 5-option trailing-unused, and a name-agnostic board (`P0..P3` labels).
- A priority change flips the board projection dirty (regression against today's local-only invariant) and enqueues a single-select projection.
- The generalized `set_single_select_option` sets the Priority field id (not Status) — wiremock/`InMemoryRemoteProjectProvider` asserts the mutation input `fieldId` + `singleSelectOptionId`.
- Priority absent on a board → no mapping derived, no projection, no error.

**Type (issue rail):**
- `RestClient::update_issue` PATCH body includes `"type"` when the DTO field is `Some`; omitted when `None`.
- Type resolution picks the right `(project_id, org, local_type)` row on a simulated multi-org board.
- Re-baseline-only-transmitted: a dropped type (simulated no-push-access) leaves the task dirty rather than hiding it.
- Org registry empty (user-owned board) → Type rail disabled, no error.

**Notices (D10):**
- A clamp collapse emits `PriorityClamped` (not an error); a priority set with no board Priority field emits `PriorityFieldMissing`.
- A `Custom(_)` type absent from the org registry emits `TypeUnmapped`; a simulated no-push-access drop emits `TypeDropped`; a user-owned org emits `TypeUnavailable`.
- Each new variant has a `sync_notice_line` prose arm and renders to stderr while stdout JSON stays clean (mirror the existing `RelationTargetUntracked` test).
- A `promote`/`push` verb populates `messages` (regression against the current `pull`-only assumption).
- Prerequisite test-infra: add `issue_type` to the recorded-update stubs (`InMemoryRemoteTaskProvider`, the `application-sync` `FakeProvider`) mirroring the RFC 0003 assignees precedent; widening `RemoteTaskUpdate` will break existing `update_issue_*` struct literals — the intended tripwire.

## Appendix A — field scoping matrix

| Field | GitHub object | Kind | Scope | Value identity | Mapping table (key → value) | Rail |
|---|---|---|---|---|---|---|
| Status | project item | single-select | project | opaque `option_id` | `project_status_mappings` (project_id, is_open) → option_id | project-item |
| **Priority** | project item | single-select | project | opaque `option_id` | `project_priority_mappings` (project_id, priority) → option_id | project-item |
| **Type** | issue | native issue type | **org** | type **name** / id | `org_issue_types` (owner_login, local_type) → name (+id) | issue (RFC 0003) |
| "Type"/"Types" (custom) | project item | single-select | project | opaque `option_id` (resolved by option **name**, #238) | — (no mapping table; name match over `project_field_options`) | project-item (`SetProjectType`, §0 A6) |

## Appendix B — current vs target field model

Current: `Project { status_field_id, status_options: Vec<StatusOption>, status_mappings: Vec<StatusMapping> }` — one hardcoded field.

Target (D2): a keyed set of project fields (`status`, `priority`), each `{ field_id, options: Vec<FieldOption> }`, plus lifecycle `status_mappings` and ordinal-derived `priority_mappings`; native issue type modeled separately (issue-side, org-vocabulary cache), not as a project field.
