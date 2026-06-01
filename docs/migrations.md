# Migration runbook

Operational guidance for repo-link's embedded SQLite migrations. This document
covers the sqlx-sqlite constraints that matter for safety, how to recover from
checksum skew, and a per-rebuild reversibility story for the release-blocking
RFC 0002 D6 migration.

## How migrations work

repo-link embeds migrations via `sqlx::migrate!("./migrations")` in
`infra-sqlite`. Migrations run automatically at process start inside
`open_db` (writer pool only, before the reader pool opens). Each migration
file is checksummed by sqlx and recorded in `_sqlx_migrations`. The migration
is skipped on subsequent starts if its checksum matches; it fails with a
checksum error if the file was edited after it was applied.

**Never edit a migration file that has already been applied** (to any
environment — including a developer's local DB). Editing a shipped file causes
`sqlx::migrate::MigrateError::VersionMismatch` on the next process start for
everyone who already applied it.

## sqlx-sqlite transaction constraint

sqlx-sqlite wraps every migration in a transaction (`Migrate::apply` calls
`self.begin()` unconditionally). The `-- no-transaction` file-level marker is
recognised only by the Postgres/MySQL drivers; SQLite ignores it.

Consequence: `PRAGMA foreign_keys = OFF` issued inside a migration is a no-op.
The PRAGMA modifies a per-connection flag; inside a transaction the connection
already has `foreign_keys = ON` (set by `open_write_pool` connect options), and
the PRAGMA cannot take effect for the duration of that transaction.

This means:

- **Parent-table rebuilds are unsafe.** Dropping a parent table with
  `foreign_keys = ON` causes an implicit CASCADE DELETE to every child table.
  Any migration that does `DROP TABLE tasks` or `DROP TABLE workspaces` will
  silently wipe tasks, snapshots, relations, comments, and remote_mappings.
- **Leaf-table rebuilds are safe.** A table that is referenced by a parent but
  has no children of its own can be dropped without cascading. `remote_mappings`
  is a leaf: only `tasks` references it (via the `task_id` FK); nothing
  references `remote_mappings`. The D6 migration exploits this property.
- **Plain ADD COLUMN is always safe.** Additive, non-destructive, runs cleanly
  inside the forced transaction, and preserves every child row, index, and
  constraint. Prefer ADD COLUMN over a rebuild wherever a real FK is not
  strictly required (application-level null-out is an acceptable substitute).

CI's `RUSTFLAGS=-D warnings` gate does not catch checksum skew or
cascade-delete data loss — those are runtime-only failures invisible to the
compiler.

## Recovering from checksum skew (local dev)

If you end up with a `VersionMismatch` error on a local DB it means the file
on disk was edited after it was applied. The local DB is now inconsistent;
the safest recovery is:

```sh
# Delete the checksum record for the affected migration so sqlx re-applies it.
# VERSION is the numeric prefix of the migration filename, e.g. 20260601000001.
sqlite3 ~/.local/share/repo-link/repo-link.db \
  "DELETE FROM _sqlx_migrations WHERE version = <VERSION>;"

# Restart the process — open_db re-runs the migration from scratch.
rl ...
```

If the re-run fails (e.g. the table already exists in a partially-applied
state), the fastest recovery is to delete the DB file entirely and let the
process recreate it from scratch. The local DB is a cache of remote state; no
data is permanently lost as long as the upstream GitHub project and issues are
intact.

## RFC 0002 D6 — remote_mappings re-key: reversibility story

**Migration file:** `20260601000001_remote_mappings_rekey_filing.sql`  
**Ticket:** #120 (schema); #126 (verification / this doc)

### What the migration does

1. Backfills `tasks.filing_repo_id = repo_id` for every remotely-backed task
   (`remote_id IS NOT NULL AND filing_repo_id IS NULL`). Idempotent via the IS
   NULL guard.
2. Rebuilds `remote_mappings` as a new table with the identity key changed from
   `(repo_id, provider, remote_id)` to `(filing_repo_id, provider, remote_id)`.
   The old `repo_id` column is replaced by `filing_repo_id NOT NULL DEFAULT ''`.
3. Creates `idx_tasks_remote_lookup` — an expression index on
   `COALESCE(filing_repo_id, repo_id)` matching the read-side dedup predicate
   in `find_by_remote`.

### Why the leaf rebuild is safe (no orphaned FKs)

`remote_mappings` is a **leaf table**: nothing references it. The cascade-delete
hazard from the forced transaction + `foreign_keys = ON` only applies to parent
tables. Dropping `remote_mappings` cannot cascade to any other table. The
INSERT … SELECT that precedes the DROP copies every row before the old table
disappears, so no data is lost.

This is confirmed by `PRAGMA foreign_key_check` in the automated test
`rfc0002_migration_sequence_data_integrity` (`infra-sqlite/tests/integration.rs`).

### Forward verification

The test `rfc0002_migration_sequence_data_integrity` in
`crates/infra-sqlite/tests/integration.rs` checks all of the following after
the full RFC 0002 migration sequence:

- Both remote_mapping rows seeded before the check survive (count = 2).
- Workspace and task rows are intact (no cascade wipe).
- `remote_mappings` has `filing_repo_id` and does NOT have `repo_id`.
- The UNIQUE constraint is on `(filing_repo_id, provider, remote_id)`.
- `workspaces`, `tasks`, and `task_snapshots` each have the new additive
  `filing_repo_id` column.
- `PRAGMA foreign_key_check` returns no rows (no orphaned FKs).

### Reversibility

D6 is **reversible only as a unit** — you cannot partially undo it. Because the
migration is a table rebuild (not a pure ADD COLUMN), the only way to reverse it
is to re-run the previous shape of `remote_mappings`.

**For a local dev DB:**

```sh
# 1. Delete the D6 migration record so sqlx no longer considers it applied.
sqlite3 <path-to-db> \
  "DELETE FROM _sqlx_migrations WHERE version = 20260601000001;"

# 2. Manually restore the pre-D6 table shape (only needed if the table is
#    already in the new shape and you need to downgrade):
sqlite3 <path-to-db> <<'SQL'
CREATE TABLE remote_mappings_restore (
    task_id                 TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    repo_id                 TEXT NOT NULL DEFAULT '',
    provider                TEXT NOT NULL,
    remote_id               TEXT NOT NULL,
    last_remote_updated_at  TEXT,
    last_synced_at          TEXT,
    UNIQUE(repo_id, provider, remote_id)
);
INSERT INTO remote_mappings_restore
    (task_id, repo_id, provider, remote_id, last_remote_updated_at, last_synced_at)
SELECT m.task_id, COALESCE(t.repo_id, ''), m.provider, m.remote_id,
       m.last_remote_updated_at, m.last_synced_at
FROM remote_mappings m
JOIN tasks t ON t.id = m.task_id;
DROP TABLE remote_mappings;
ALTER TABLE remote_mappings_restore RENAME TO remote_mappings;
DROP INDEX IF EXISTS idx_tasks_remote_lookup;
SQL

# 3. Restart the process. open_db will re-run the migration from scratch
#    against the restored shape.
```

**For a production release rollback (if D6 shipped to users):**

D6 is data-preserving in both directions: the backfill sets `filing_repo_id =
repo_id` for all existing rows, so the stored values are identical under both
key shapes. A rollback that restores `repo_id` from `filing_repo_id` loses no
information.

Steps:

1. Ship a binary that includes the pre-D6 migration set (i.e. the binary from
   before the release).
2. On each affected machine, delete the D6 checksum record and run the manual
   restore SQL above before starting the old binary.
3. The old binary's `open_db` will attempt to re-apply D6 (since the record was
   deleted). If the intent is to stay on the old schema, delete the D6 migration
   file from the release build entirely and ship a hotfix binary.

In practice, because repo-link is a local CLI tool with a per-user SQLite DB,
"production rollback" means the user runs a one-liner and restarts. The D6
migration leaves no ambiguity: `filing_repo_id` values are identical to
`repo_id` values for all rows that existed before the RFC 0002 wave, so
restoring the old key is lossless.

### What NOT to do

- Do not edit `20260601000001_remote_mappings_rekey_filing.sql` after it has
  shipped — that breaks sqlx checksums for everyone who already applied it.
- Do not attempt `ALTER TABLE remote_mappings ADD COLUMN repo_id` as a partial
  undo — the UNIQUE constraint is on the wrong column set and cannot be altered
  in place in SQLite.
- Do not run the restore SQL inside a manually opened transaction with
  `PRAGMA foreign_keys = OFF` — on the sqlx write pool `foreign_keys = ON` is
  set at connection-open time and cannot be toggled inside a transaction. Run it
  via the `sqlite3` CLI (outside the app process) where you can control the
  session flags.
