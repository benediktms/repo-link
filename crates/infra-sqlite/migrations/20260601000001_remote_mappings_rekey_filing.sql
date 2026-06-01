-- RFC 0002 D6 — re-key remote identity to the FILING repo, plus the section-3
-- backfill. These MUST land together (and in the one change that first lets
-- filing diverge from logical in production): remote dedup has a read side
-- (find_by_remote, in task_repo.rs) and a write side (the remote_mappings
-- UNIQUE key). If they disagree, dedup silently misses or false-duplicates once
-- a task's filing repo differs from its logical repo.
--
-- ORDER MATTERS: backfill tasks.filing_repo_id FIRST, then rebuild
-- remote_mappings, so the rebuild's COALESCE reads the freshly-backfilled value.

-- (1) Section-3 backfill. Every task with a real backing issue historically
-- filed in its logical repo, so record that as the resolved filing repo. This
-- makes the recorded target authoritative and immune to a later workspace
-- default change retargeting an existing issue. Purely-local tasks and board
-- drafts stay NULL (they resolve via the D2 chain at their first filing); an
-- orphan draft's repo_id is NULL anyway, so the remote_id condition is the
-- meaningful guard. Idempotent via the IS NULL clause.
UPDATE tasks
SET filing_repo_id = repo_id
WHERE remote_id IS NOT NULL AND filing_repo_id IS NULL;

-- (2) Rebuild remote_mappings with the key (filing_repo_id, provider,
-- remote_id) instead of (repo_id, provider, remote_id). Mirrors the rebuild
-- established by 20260527000001_repo_scope_remote_mappings.sql.
--
-- SAFETY: remote_mappings is a LEAF table — only `tasks` is referenced by it
-- (task_id FK), and NOTHING references remote_mappings. So DROP-ing it does not
-- cascade to any other table. (This is why a rename-copy-drop is safe here even
-- though sqlx-sqlite forces every migration into a transaction where
-- PRAGMA foreign_keys=OFF is a no-op — a parent-table rebuild would CASCADE-wipe
-- children, but a leaf cannot. We do not toggle the PRAGMA: it would be a no-op.)
--
-- filing_repo_id is NOT NULL DEFAULT '' (not nullable): it is part of the UNIQUE
-- key and SQLite treats NULLs as distinct, so a nullable column would let
-- duplicates slip past the constraint. The empty-string sentinel keeps the key
-- well-defined for the degenerate repo-less remote task (allowed at the storage
-- layer, though such a task can't actually sync). COALESCE prefers the resolved
-- filing repo, falling back to the logical repo for any row the step-1 backfill
-- did not touch (there should be none with a non-NULL remote_id, but the
-- fallback keeps the rebuild total).
CREATE TABLE remote_mappings_new (
    task_id                 TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    filing_repo_id          TEXT NOT NULL DEFAULT '',
    provider                TEXT NOT NULL,
    remote_id               TEXT NOT NULL,
    last_remote_updated_at  TEXT,
    last_synced_at          TEXT,
    UNIQUE(filing_repo_id, provider, remote_id)
);

INSERT INTO remote_mappings_new
    (task_id, filing_repo_id, provider, remote_id, last_remote_updated_at, last_synced_at)
SELECT m.task_id, COALESCE(t.filing_repo_id, t.repo_id, ''), m.provider, m.remote_id, m.last_remote_updated_at, m.last_synced_at
FROM remote_mappings m
JOIN tasks t ON t.id = m.task_id;

DROP TABLE remote_mappings;
ALTER TABLE remote_mappings_new RENAME TO remote_mappings;

-- (3) Expression index for the re-keyed read-side dedup. find_by_remote now
-- filters on `COALESCE(filing_repo_id, repo_id)`, which is non-sargable against
-- the plain `idx_tasks_repo(repo_id)` index — the function-wrapped column would
-- force a full `tasks` scan on every `sync import` dedup as the table grows.
-- This expression index matches the predicate exactly (same COALESCE, same
-- provider/remote_id ordering) so the lookup stays O(log N). Partial on
-- `remote_provider IS NOT NULL` because only remote-backed rows can match,
-- keeping the index off the local-only majority.
CREATE INDEX idx_tasks_remote_lookup
    ON tasks (COALESCE(filing_repo_id, repo_id), remote_provider, remote_id)
    WHERE remote_provider IS NOT NULL;
