# RFC 0005 — Shared repo identity across workspaces

Status: Proposed (2026-06-29)
Tracking issue: **#202**
Relates to: RFC 0002 (logical vs. filing repo axes — the origin owns the logical
identity and prefix); RFC 0001 (project sync); `docs/migrations.md` (sqlx-sqlite
migration-safety runbook + manual down-path convention).

## 1. Context

Attaching the same on-disk repo (same `canonical_url`) to more than one workspace
creates a **duplicate `repos` row** — same code on disk, two DB rows with two
different `RepoId`s. The divergence propagates:

- the two rows drift in `name` / `aliases` / `tracked_branch`, and
- each gets its **own task-ID prefix** (collision-breaking yields `rpl` in one
  workspace, `rpl1` in another), so tasks for the *same* repo carry different
  friendly IDs depending on which workspace created them.

### Why it happens (verified against the current schema)

- `repos` is **workspace-scoped**: `UNIQUE(workspace_id, canonical_url)`,
  `workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE`, plus
  `idx_repos_workspace`. There is no workspace↔repo join — the row is owned by
  exactly one workspace.
- `prefix` is **globally unique** (`idx_repos_prefix … WHERE prefix != ''`) but
  lives on that per-workspace row. That is the core contradiction: a
  cross-workspace-unique value pinned to a per-workspace entity.
- `RepoBindingService::attach` looks up an existing row via
  `find_by_canonical_url(workspace_id, canonical_url)` (scoped to one workspace).
  A second workspace misses → `RepoBinding::new` mints a fresh `RepoId` and
  `save_with_unique_prefix` inserts a duplicate row with a collision-broken prefix.
- `rl repo locate` and `memberships_for_canonical_url` already iterate **all**
  non-archived workspaces and match by `canonical_url` — reads already treat the
  canonical URL as the cross-workspace identity. Only storage fragments it.

### What references `repos` today

| Column | Real FK to `repos`? | Axis |
|--------|---------------------|------|
| `tasks.repo_id` | yes (`ON DELETE SET NULL`, indexed `idx_tasks_repo`) | logical / work |
| `worktree_links.repo_id` | yes (`ON DELETE CASCADE`, in PK) | physical checkout |
| `tasks.filing_repo_id` | no — plain `TEXT` | filing |
| `workspaces.filing_repo_id` | no — plain `TEXT` | filing default |
| `remote_mappings.filing_repo_id` | no — plain `TEXT`, in `UNIQUE(…)` | filing / remote key |
| `task_snapshots.repo_id` / `filing_repo_id` | no — plain `TEXT`, audit only | rollback record |

Only **two** columns are hard FKs. That fact is what makes the migration
additive (see D6).

### Goal

Make the same on-disk repo a **single shared entity** across workspaces, so
attaching it to a second workspace reuses the existing identity (consistent
`prefix` → consistent friendly IDs) instead of duplicating it.

## Terminology

This RFC adds two terms; it keeps RFC 0002's **logical repo** / **filing repo**
vocabulary unchanged.

- **Repo origin** — the shared identity of a repo, keyed on `canonical_url`. Owns
  everything intrinsic to "the same code": `prefix`, `name`, `aliases`,
  `remote_url`. One origin per canonical URL, across all workspaces. Handle
  shorthands (`aliases`) are repo-global — resolution is cross-workspace, so they
  belong to the identity, not a membership. The origin **is** the logical repo of
  RFC 0002 — it is the source of the friendly-ID prefix — and it is the unit of
  **remote identity** (the GitHub issue lives in a canonical repo; see D4).
- **Repo instance** — a workspace's *membership* of an origin: the `(workspace,
  origin)` pair plus per-workspace state (`tracked_branch`, worktrees). This is
  what today's `repos` row already is (it is per-workspace); RFC 0005 renames it
  and strips the *identity* fields out to the origin.

## 2. Decisions

### D1 — Two tables: `repo_origins` (identity) + `repo_instances` (membership)

Split repo identity from workspace membership.

`repo_origins` — one row per `canonical_url`:

| Field | Notes |
|-------|-------|
| `id` | `RepoOriginId` |
| `canonical_url` | `UNIQUE` — the cross-workspace identity |
| `remote_url` | intrinsic to the remote |
| `prefix` | globally unique; drives task IDs |
| `name` | one canonical display name per repo (survivor rule in D6) |
| `aliases` | repo-global handle shorthands; same JSON-array CHECK as today; survivor rule unions all instances' aliases (D6) |
| `created_at`, `updated_at` | |

`repo_instances` — one row per `(workspace_id, origin_id)` (this is the renamed
`repos` table; see D6):

| Field | Notes |
|-------|-------|
| `id` | `RepoInstanceId` — the existing `RepoId`, preserved (no churn to task FKs) |
| `workspace_id` | `REFERENCES workspaces(id) ON DELETE CASCADE` — the instance's real identity |
| `origin_id` | `REFERENCES repo_origins(id)` |
| `tracked_branch` | per-workspace (a workspace may track a different branch) |
| `canonical_url` | retained, denormalized — backs the still-valid `UNIQUE(workspace_id, canonical_url)` (one membership per workspace per repo) and is a convenient join/debug key |
| `created_at`, `updated_at` | |

The domain `RepoBinding` aggregate splits along this seam: identity fields become
`RepoOrigin`; the membership becomes `RepoInstance`. The cross-aggregate invariant
(D7) is: **prefix/name/aliases live on the origin; `(workspace, origin)`
uniqueness and worktrees/branch live on the instance; a task's display ID needs
both** (the instance for existence/workspace, the origin for the prefix).

### D2 — Reference split: which entity each reference resolves to

References to the old `repos` row fall into buckets that resolve to *different*
entities. Collapsing them all onto one entity would re-introduce the
fragmentation we are removing.

- **Logical / work + physical checkout → instance.** `tasks.repo_id` (renamed
  `repo_instance_id`); `worktree_links.repo_id` (a checkout is registered by the
  workspace whose user cloned it — kept instance-scoped, matching today's
  behaviour exactly; per-origin worktree dedup is explicitly *not* attempted here,
  see Out of scope).
- **Filing / remote → origin.** `tasks.filing_repo_id`, `workspaces.filing_repo_id`,
  `remote_mappings.filing_repo_id`. The filing repo is a canonical issue
  destination that can live in a workspace the task is not even a member of (RFC
  0002's shared `team-eng` issues repo), and remote identity is origin-level
  (D4). These are soft `TEXT` references today and stay soft; the migration
  rewrites their **values** to origin ids.
- **Audit only → left as-is.** `task_snapshots.repo_id` / `filing_repo_id` are
  rollback/audit records, not live FKs. The migration does **not** rewrite them;
  rollback compares recorded values, and `repo_id_recorded` already distinguishes
  "unknown" from "known-null".

Consequence: a task's friendly-ID prefix resolves `task → instance → origin →
prefix` (one hop more than today's `task → repo → prefix`). A NULL
`repo_instance_id` (orphan task) **short-circuits to the bare-hash display ID
before attempting the origin hop** — the added indirection must not introduce a
new None-join surface (D7; tripwire in D6).

### D3 — Prefix lives on the origin; consistent task IDs

Moving `prefix` to the origin is the fix for the reported bug: every instance of
the same repo shares one prefix, so tasks carry the same friendly ID regardless
of which workspace created them.

The cost is **not** "display only": `resolve_task` verifies an input prefix against
the task's current prefix and **hard-errors `PrefixMismatch`** on a mismatch. After
the migration, a task created under a collision-broken instance (`rpl1`) resolves
through the origin prefix (`rpl`), so any composite ID a user/agent already typed
into a commit message, branch name, or PR body as `rpl1-ak7` would hard-error.

Decision: `resolve_task` must **tolerate a recognised superseded prefix** — when
the input prefix is a known collision-suffixed variant of the task's origin prefix
(`rpl1` for an origin whose prefix is `rpl`), resolve with a deprecation warning
instead of erroring, for a transition window. The UUID primary keys are unchanged,
so anything storing a task UUID is unaffected; this concerns only the
human-typed composite IDs. Document the change in CLI help, mirroring the existing
`set_prefix` staleness warning.

### D4 — Remote identity is origin-level (the dedup/doctor coupling)

A GitHub issue lives in a canonical repo, so the **remote-identity key is
origin-level**, end to end. This is not separable from the schema split: the
read-side dedup predicate `find_by_remote` uses `COALESCE(filing_repo_id, repo_id)`
and the `idx_tasks_remote_lookup` expression index mirrors it. If `filing_repo_id`
becomes an origin id while `repo_id` (→ `repo_instance_id`) stays an instance id,
the COALESCE compares ids of **different entities** — dedup silently misses
duplicates or never collides across workspaces. (RFC 0002 D6 warned of exactly
this read/write key disagreement.)

Decision: the entire remote-identity key operates in **origin id space**:

- `remote_mappings.filing_repo_id` stores an **origin id**; its
  `UNIQUE(filing_repo_id, provider, remote_id)` becomes a cross-workspace
  uniqueness guard (one remote issue = one mapping, correct for a shared filing
  origin).
- **As built:** `find_by_remote` and `idx_tasks_remote_lookup` key on
  `filing_repo_id` **alone** (an origin id) — the old
  `COALESCE(filing_repo_id, repo_id)` fallback is dropped. RFC 0002 D6 already
  backfilled `filing_repo_id` for every remote-backed task, and this migration's
  straggler backfill (§D6 step 7a) covers any gap, so the logical fallback was
  dead for indexed rows and the predicate stays in one (origin) id space with no
  denormalized column. This resolves Open-Q1 — neither a denormalized
  `tasks` column nor a `remote_mappings`-UNIQUE read path was needed.
- `find_by_remote_mapping` / `resolve_doctor_target` must JOIN
  `remote_mappings.filing_repo_id` to **`repo_origins(id)`** (or through
  `repo_instances.origin_id`). "Dangling filing" now means *no surviving origin*,
  not *no instance* — the doctor's full redesign is in D7.

This decision is the reason remote_mappings is **not** "out of scope": the COALESCE
fallback structurally couples it to the split, so it migrates in the same change.

### D5 — Delete / detach semantics

- **Detach a repo from a workspace** → delete the `repo_instances` row only.
  `tasks.repo_instance_id` is `ON DELETE SET NULL`, so those tasks become orphans
  (logical repo unknown), exactly as a detached `repo_id` behaves today. The
  origin survives.
- **Delete a workspace** → `repo_instances.workspace_id ON DELETE CASCADE` removes
  that workspace's instances (and cascades their `worktree_links`). Origins are
  **not** workspace-scoped and survive.
- **Origins are never auto-deleted** by detach/workspace-delete. An origin with no
  remaining instances and no referencing tasks is a GC candidate, but GC is out of
  scope for the first cut (an idle origin costs one row).

### D6 — Migration: additive rename, no table rebuild

The whole reshape is achievable with `ALTER TABLE … RENAME`, `ADD COLUMN`,
`DROP COLUMN`, `CREATE`, and `UPDATE` — **no parent-table rebuild**, so it runs as
an ordinary sqlx migration inside the forced FK-on transaction.

The load-bearing trick: **`repos` is already the per-workspace membership row, so
it already is the instance.** Renaming it carries the existing FKs along for free.
Confirmed empirically on SQLite 3.51 — `ALTER TABLE repos RENAME TO repo_instances`
inside a FK-on transaction rewrites the FKs of `tasks.repo_id` (ON DELETE SET NULL)
**and** `worktree_links.repo_id` (ON DELETE CASCADE) to `REFERENCES
"repo_instances"(id)`, leaves data intact, passes `foreign_key_check`, and keeps
the FKs enforced (SQLite ≥ 3.25 behaviour; bundled driver is 3.51). The later
`ALTER TABLE tasks RENAME COLUMN repo_id TO repo_instance_id` is metadata-only and
**auto-rewrites both `idx_tasks_repo` and the `COALESCE(...)` expression index
`idx_tasks_remote_lookup`** (verified on 3.51).

**Tripwire tests must pin** (the migration's safety rests on these SQLite
behaviours): post-rename `tasks` *and* `worktree_links` FKs target
`repo_instances`; `idx_tasks_remote_lookup`'s definition contains
`repo_instance_id`; `foreign_key_check` returns no rows; an orphan task's display
ID and a `resolve_task` round-trip still work.

Ordering (each step additive / non-cascading):

```sql
-- 1. Rename: repos IS the instance. tasks.repo_id + worktree_links.repo_id
--    FKs auto-follow to repo_instances. No worktree rebuild needed (worktrees
--    stay instance-scoped, D2).
ALTER TABLE repos RENAME TO repo_instances;

-- 2. Shared-identity table.
CREATE TABLE repo_origins (
    id            TEXT PRIMARY KEY,
    canonical_url TEXT NOT NULL UNIQUE,
    remote_url    TEXT NOT NULL,
    prefix        TEXT NOT NULL,
    name          TEXT NOT NULL DEFAULT '',
    aliases       TEXT NOT NULL DEFAULT '[]'
                  CHECK (json_valid(aliases) AND json_type(aliases) = 'array'),
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_origins_prefix ON repo_origins(prefix);

-- 3. One origin per distinct canonical_url. SURVIVOR RULE (a data-loss decision,
--    not an implementation detail): earliest created_at wins prefix AND name
--    (the first-attached instance held the un-suffixed prefix); aliases are the
--    UNION across all instances of the group. Any DISTINCT non-surviving name is
--    NOT discarded — it is folded into the origin's `aliases` so a user-chosen
--    rename stays resolvable, and the migration logs the folded names.
-- The shipped migration picks the survivor with a correlated subquery rather
-- than GROUP BY, so the non-grouped columns are deterministic (a bare GROUP BY
-- canonical_url would leave prefix/name/created_at indeterminate):
INSERT INTO repo_origins (id, canonical_url, remote_url, prefix, name, aliases, created_at, updated_at)
SELECT s.id, s.canonical_url, s.remote_url, s.prefix, s.name, s.aliases, s.created_at, s.updated_at
FROM repo_instances s
WHERE s.id = (SELECT s2.id FROM repo_instances s2
              WHERE s2.canonical_url = s.canonical_url
              ORDER BY s2.created_at ASC, s2.id ASC LIMIT 1);
-- aliases are then unioned across the group + non-surviving names folded in.

-- 4. Link instance → origin. ADD COLUMN with a FK is legal because the default is
--    NULL; backfill by canonical_url. The column stays NOMINALLY NULLABLE at the
--    schema level (SQLite cannot add a NOT NULL FK column without a default);
--    non-null is enforced by the attach path, not the schema.
ALTER TABLE repo_instances ADD COLUMN origin_id TEXT REFERENCES repo_origins(id);
UPDATE repo_instances
   SET origin_id = (SELECT o.id FROM repo_origins o WHERE o.canonical_url = repo_instances.canonical_url);

-- 5. Identity now lives on the origin → drop the redundant instance columns.
--    `prefix` drops only AFTER its unique index. `name` and `aliases` drop
--    cleanly. NB: `aliases` carries a CHECK (json_valid…); DROP COLUMN of a
--    CHECK'd column is safe ONLY because that CHECK is self-contained (names no
--    other column) — verified on 3.51. repo_origins re-declares the same CHECK.
DROP INDEX idx_repos_prefix;
ALTER TABLE repo_instances DROP COLUMN prefix;
ALTER TABLE repo_instances DROP COLUMN name;
ALTER TABLE repo_instances DROP COLUMN aliases;

-- 6. Rename tasks' work-axis column for honesty (metadata-only; rewrites the two
--    indexes automatically). Re-point any task whose instance was merged away
--    under dedup to the surviving instance for the same (workspace, origin).
UPDATE tasks SET repo_id = <surviving instance id> WHERE repo_id IN (<merged-away instance ids>);
ALTER TABLE tasks RENAME COLUMN repo_id TO repo_instance_id;

-- 7. Remote/filing axis → ORIGIN id space (D4). Soft TEXT refs: rewrite VALUES.
--    remote_mappings is the sharp one: collapsing instance ids to origin ids can
--    VIOLATE UNIQUE(filing_repo_id, provider, remote_id) if two workspaces had a
--    mapping for the same remote issue under the shared filing repo. The
--    migration MUST first assert no such collision:
--      SELECT 1 FROM (SELECT <origin_of(filing_repo_id)> oid, provider, remote_id
--                     FROM remote_mappings GROUP BY oid, provider, remote_id
--                     HAVING COUNT(*) > 1) LIMIT 1;   -- must be empty
--    If non-empty, apply a survivor rule (keep the most-recently-synced mapping)
--    BEFORE the rewrite. Then:
UPDATE tasks            SET filing_repo_id = <origin id> WHERE filing_repo_id IS NOT NULL;
UPDATE workspaces       SET filing_repo_id = <origin id> WHERE filing_repo_id IS NOT NULL;
UPDATE remote_mappings  SET filing_repo_id = <origin id> WHERE filing_repo_id != '';

-- 8. PRAGMA foreign_key_check  -- must return no rows.
```

#### Back up the database before running this migration

The migration is **forward-only** (sqlx `migrate!()` never reverts) and rewrites
identity-bearing rows in place. Take a backup first.

```sh
# 1. Stop the daemon so nothing writes mid-backup.
rl daemon stop

# 2. Snapshot the DB (the .backup command checkpoints the WAL, unlike a raw cp).
DB=~/.local/share/repo-link/repo-link.db
sqlite3 "$DB" ".backup '${DB}.pre-rfc0005.bak'"

# 3. Run the upgraded binary once (open_db applies the migration), then resume.
rl repo locate --path .   # or any command — triggers open_db
rl daemon start
```

Recovery if the migration misbehaves: stop the daemon, restore the snapshot
(`cp "${DB}.pre-rfc0005.bak" "$DB"`, removing any `-wal`/`-shm` siblings), and run
the previous binary. As a last resort the DB is a re-syncable cache of GitHub —
deleting it and re-syncing loses only local-only (never-promoted) tasks.

#### Manual down-path

Per the `docs/migrations.md` convention for release-blocking migrations, the
reverse is documented but manual: recreate the pre-0005 `repos` shape from
`repo_instances` + `repo_origins` (fold `origin.prefix`/`name`/`aliases` back onto
each instance row, rename `repo_instances` → `repos`, rename `tasks.repo_instance_id`
→ `repo_id`, rewrite the filing/remote ids back, drop `repo_origins`). Add the
exact SQL to `docs/migrations.md` when the migration lands, mirroring the existing
**"RFC 0002 D6 — remote_mappings re-key: reversibility story"** section there
(not this RFC's D-numbering).

### D7 — Code-change inventory

The schema change is small; the code change is "every repo lookup must declare
whether it wants the **instance** (workspace-scoped) or the **origin** (identity),
and the remote-identity key is origin-wide." Categories (exact call sites become
implementation tickets):

- **`RepoBindingService::attach`** — the behavioral fix. Resolve/create the
  **origin** by `canonical_url` first (reuse if present → consistent prefix), then
  upsert the `(workspace, origin)` **instance**. A second workspace reuses the
  origin instead of minting a duplicate.
- **`save_with_unique_prefix`** — its conflict-break catches
  `conflict_target() == Some("repos.prefix")`. The unique index moves to
  `repo_origins`, so this match string goes stale and the retry loop silently
  stops firing (raw UNIQUE error propagates). Update the match to the new origin
  index target and add a test that the retry still fires post-split.
- **`find_by_canonical_url` / `memberships_for_canonical_url` / `rl repo locate` /
  `resolve_by_handle` / `find`** — origin lookups; `resolve_by_handle`/`find`
  rank on `name`/`canonical_url`/`aliases`, all now origin-level, so they query
  `repo_origins` directly (no instance join for handle resolution).
- **Prefix / friendly-ID generation** — resolves through the origin (`instance →
  origin → prefix`); NULL instance short-circuits to bare-hash (D2).
  `resolve_task` gains superseded-prefix tolerance (D3).
- **Remote identity** (`find_by_remote`, `idx_tasks_remote_lookup`,
  `remote_mappings` UNIQUE, `find_by_remote_mapping`, the drainer, promote/pull) —
  all move to **origin id space** (D4), migrated together.
- **`rl repo doctor` / `FilingRepoRepair`** — filing now points at origins; the
  `find_by_remote_mapping` JOIN targets `repo_origins`, and "dangling" means "no
  surviving origin". Full redesign required, not a one-liner.
- **DTO / JSON / CLI surface** — `RepoBinding` DTO, `AttachRepoCmd`, `rl repo
  locate` output. Decide which ids surface (origin vs instance) and source the
  friendly-ID prefix from the origin.
- **`*_COLS` consts + `schema_const_consistency` (#110)** — the `repo_instances`
  column const must add `origin_id` and drop `prefix`/`name`; the project's
  schema-const test will otherwise break reads (a known recurring miss).
- **Domain split** — `RepoBinding` → `RepoOrigin` + `RepoInstance` aggregates,
  with the cross-aggregate invariant from D1.

## 3. Open questions

1. ~~**Remote-identity fast path (D4).**~~ **Resolved** — neither option was
   needed: `filing_repo_id` is backfilled for every remote-backed task, so the
   index keys on it alone (origin id space), no denormalized column. See §D4.
2. **`tracked_branch` placement.** Kept per-instance. If it always agrees across a
   repo's instances, it could move to the origin — defer until there's evidence.
3. **`#112` terminology prerequisite (RFC 0002).** This RFC introduces
   `repo_instance_id`; align the rename with the logical/filing vocabulary pass so
   a bare `repo` isn't ambiguous.
4. **Origin GC.** Out of scope here; revisit if idle origins accumulate.

## 4. Out of scope

- Behavioural changes to sync / poller / drainer beyond repo identity and the
  origin-keyed remote-identity migration (D4).
- **Per-origin worktree dedup.** Worktrees stay instance-scoped (D2), so a checkout
  attached to two workspaces keeps two `worktree_links` rows exactly as today;
  `reconcile_worktrees` stays workspace-scoped. Unifying worktrees onto the origin
  (and making reconcile origin-scoped without two workspaces racing on physical
  truth) is deferred.
- Origin garbage collection.
