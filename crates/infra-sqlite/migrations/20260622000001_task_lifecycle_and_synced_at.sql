-- RFC 0004 Phase 1 — collapse the 5-state `TaskStatus` into a single
-- `lifecycle` value, and add the write-through `synced_at` cache column.
--
-- ## `lifecycle` (RFC 0004 D1)
--
-- The local lifecycle is now one closed value — `open | reopened | completed
-- | not_planned` — mapping 1:1 to GitHub's REST `(state, state_reason)`. The
-- old `status` (`open | in_progress | blocked | done | archived`) is replaced.
--
-- `tasks` is a PARENT table: `task_snapshots`, `task_relations`, and
-- `remote_mappings` reference it `ON DELETE CASCADE`. SQLite cannot ALTER a
-- CHECK in place, and the usual rename-copy-DROP rebuild is UNSAFE here —
-- sqlx runs each migration in a forced transaction where `PRAGMA
-- foreign_keys = OFF` is a no-op, so DROPping `tasks` would cascade-delete all
-- snapshots/relations. So this is strictly ADD COLUMN:
--   * add `lifecycle` (its own CHECK), backfilled from `status`;
--   * the legacy `status` column STAYS. The repository no longer READS it, but
--     still WRITES a derived legacy value (open/reopened→'open',
--     completed→'done', not_planned→'archived') so its NOT NULL CHECK and the
--     `idx_tasks_status` index keep holding. A future cleanup migration can
--     retire `status` once nothing depends on it.
-- `in_progress` and `blocked` both collapse to `open` (InProgress is gone;
-- Blocked is now a `blocked_by` relation, not a lifecycle state).
ALTER TABLE tasks ADD COLUMN lifecycle TEXT NOT NULL DEFAULT 'open'
    CHECK (lifecycle IN ('open', 'reopened', 'completed', 'not_planned'));
UPDATE tasks SET lifecycle = CASE status
    WHEN 'done' THEN 'completed'
    WHEN 'archived' THEN 'not_planned'
    ELSE 'open'
END;
-- `list()` now filters `WHERE lifecycle IN (...)`; the legacy `idx_tasks_status`
-- indexes a column no query reads anymore, so add the matching lifecycle index.
CREATE INDEX idx_tasks_lifecycle ON tasks(lifecycle);

-- ## `synced_at` (RFC 0004 D3)
--
-- Write-through "remote last observed" timestamp. Nullable, NO backfill:
-- every existing task starts NULL = "never observed". The poller's first
-- post-migration tick re-fetches every mirrored task (one burst, then steady
-- state). Plain ADD COLUMN — no CHECK, no NOT NULL, no rebuild.
ALTER TABLE tasks ADD COLUMN synced_at TEXT;

-- `task_snapshots` carries the same lifecycle axis (append-only history). It is
-- a CHILD table (safe to rebuild), but ADD COLUMN keeps the migration uniform
-- and additive. The legacy `status` column stays for the same reason as above.
ALTER TABLE task_snapshots ADD COLUMN lifecycle TEXT NOT NULL DEFAULT 'open'
    CHECK (lifecycle IN ('open', 'reopened', 'completed', 'not_planned'));
UPDATE task_snapshots SET lifecycle = CASE status
    WHEN 'done' THEN 'completed'
    WHEN 'archived' THEN 'not_planned'
    ELSE 'open'
END;
