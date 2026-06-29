-- RFC 0005 — shared repo identity across workspaces.
--
-- Splits repo identity (ORIGIN: keyed on canonical_url, owns prefix/name/aliases/
-- remote_url) from workspace membership (INSTANCE: the per-(workspace, repo) row).
-- See docs/rfcs/0005-shared-repo-identity.md.
--
-- ADDITIVE — no parent-table rebuild. `repos` is already the per-workspace
-- membership row, so renaming it to `repo_instances` carries the child FKs
-- (tasks.repo_id, worktree_links.repo_id) along automatically (SQLite >= 3.25
-- rewrites referencing FK definitions on RENAME; verified on 3.51). All instance
-- rows are KEPT — only the shared identity is extracted into repo_origins, so no
-- task ever needs re-pointing.

------------------------------------------------------------------------------
-- 1. repos IS the instance → rename. tasks.repo_id + worktree_links.repo_id FKs
--    auto-follow to repo_instances; idx_repos_prefix / idx_repos_workspace ride along.
------------------------------------------------------------------------------
ALTER TABLE repos RENAME TO repo_instances;

------------------------------------------------------------------------------
-- 2. Shared identity table. aliases CHECK mirrors the live `repos` CHECK exactly.
------------------------------------------------------------------------------
CREATE TABLE repo_origins (
    id            TEXT PRIMARY KEY,
    canonical_url TEXT NOT NULL UNIQUE,
    remote_url    TEXT NOT NULL,
    prefix        TEXT NOT NULL DEFAULT '',
    name          TEXT NOT NULL DEFAULT '',
    aliases       TEXT NOT NULL DEFAULT '[]'
                  CHECK (json_valid(aliases) AND json_type(aliases) = 'array'),
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_origins_prefix ON repo_origins(prefix) WHERE prefix != '';

------------------------------------------------------------------------------
-- 3. One origin per canonical_url. SURVIVOR = earliest-created instance (it held
--    the un-suffixed prefix). Reuse the survivor's id as the origin id (different
--    table → no collision; deterministic without minting UUIDs in SQL).
------------------------------------------------------------------------------
INSERT INTO repo_origins (id, canonical_url, remote_url, prefix, name, aliases, created_at, updated_at)
SELECT s.id, s.canonical_url, s.remote_url, s.prefix, s.name, s.aliases, s.created_at, s.updated_at
FROM repo_instances s
WHERE s.id = (
    SELECT s2.id FROM repo_instances s2
    WHERE s2.canonical_url = s.canonical_url
    ORDER BY s2.created_at ASC, s2.id ASC
    LIMIT 1
);

-- 3b. Union aliases across all instances of each origin, and fold any DISTINCT
--     non-surviving instance name into the origin's aliases so a user-chosen
--     rename stays resolvable (RFC 0005 §D6 survivor rule).
UPDATE repo_origins
SET aliases = COALESCE((
    SELECT json_group_array(v) FROM (
        SELECT DISTINCT je.value AS v
        FROM repo_instances ri, json_each(ri.aliases) je
        WHERE ri.canonical_url = repo_origins.canonical_url
        UNION
        SELECT ri.name
        FROM repo_instances ri
        WHERE ri.canonical_url = repo_origins.canonical_url
          AND ri.name != '' AND ri.name != repo_origins.name
    )
), '[]');

------------------------------------------------------------------------------
-- 4. Link instance → origin. ADD COLUMN + FK is legal (NULL default); backfill by
--    canonical_url. Stays nominally nullable (SQLite cannot add a NOT NULL FK
--    column without a default); non-null is enforced by the attach path.
------------------------------------------------------------------------------
ALTER TABLE repo_instances ADD COLUMN origin_id TEXT REFERENCES repo_origins(id);
UPDATE repo_instances
SET origin_id = (SELECT o.id FROM repo_origins o WHERE o.canonical_url = repo_instances.canonical_url);

------------------------------------------------------------------------------
-- 5. Identity lives on the origin now → drop the redundant instance columns.
--    `prefix` only after its unique index. `aliases`' CHECK is self-contained
--    (names only `aliases`), so DROP COLUMN removes column + CHECK together.
------------------------------------------------------------------------------
DROP INDEX idx_repos_prefix;
ALTER TABLE repo_instances DROP COLUMN prefix;
ALTER TABLE repo_instances DROP COLUMN name;
ALTER TABLE repo_instances DROP COLUMN aliases;
ALTER TABLE repo_instances DROP COLUMN remote_url;

------------------------------------------------------------------------------
-- 6. Honest column name (metadata-only; auto-rewrites idx_tasks_repo and the
--    idx_tasks_remote_lookup expression index to repo_instance_id). Instances are
--    NOT merged, so no task re-pointing — each task keeps its workspace's instance.
------------------------------------------------------------------------------
ALTER TABLE tasks RENAME COLUMN repo_id TO repo_instance_id;

------------------------------------------------------------------------------
-- 7. Remote-identity axis → ORIGIN id space (RFC 0005 §D4). filing_repo_id holds
--    an instance id today; rewrite to that instance's origin id. NB: an origin id
--    equals its survivor instance's id, so rewriting an already-origin value is
--    idempotent (survivor.origin_id == survivor.id).
------------------------------------------------------------------------------
-- 7a. Straggler backfill: a remote-backed task missing filing_repo_id resolves to
--     its logical instance's origin (RFC 0002 D6 backfilled most; this covers any
--     gap so the origin-only key in step 8 is always populated).
UPDATE tasks
SET filing_repo_id = (SELECT ri.origin_id FROM repo_instances ri WHERE ri.id = tasks.repo_instance_id)
WHERE remote_id IS NOT NULL
  AND (filing_repo_id IS NULL OR filing_repo_id = '')
  AND repo_instance_id IS NOT NULL;

-- 7b. tasks.filing_repo_id: instance id → origin id.
UPDATE tasks
SET filing_repo_id = (SELECT ri.origin_id FROM repo_instances ri WHERE ri.id = tasks.filing_repo_id)
WHERE filing_repo_id IS NOT NULL AND filing_repo_id != ''
  AND filing_repo_id IN (SELECT id FROM repo_instances);

-- 7c. workspaces.filing_repo_id: instance id → origin id.
UPDATE workspaces
SET filing_repo_id = (SELECT ri.origin_id FROM repo_instances ri WHERE ri.id = workspaces.filing_repo_id)
WHERE filing_repo_id IS NOT NULL AND filing_repo_id != ''
  AND filing_repo_id IN (SELECT id FROM repo_instances);

-- 7d. remote_mappings.filing_repo_id: instance id → origin id. Collapsing instance
--     ids to origin ids can violate UNIQUE(filing_repo_id, provider, remote_id) if
--     two workspaces mirror the same remote issue into the shared filing repo.
--     Delete the loser (keep most-recently-synced) BEFORE the rewrite.
DELETE FROM remote_mappings
WHERE task_id IN (
    SELECT m.task_id
    FROM remote_mappings m
    JOIN repo_instances ri ON ri.id = m.filing_repo_id
    WHERE EXISTS (
        SELECT 1 FROM remote_mappings m2
        JOIN repo_instances ri2 ON ri2.id = m2.filing_repo_id
        WHERE ri2.origin_id = ri.origin_id
          AND m2.provider = m.provider AND m2.remote_id = m.remote_id
          AND m2.task_id != m.task_id
          AND ( COALESCE(m2.last_synced_at,'') > COALESCE(m.last_synced_at,'')
             OR (COALESCE(m2.last_synced_at,'') = COALESCE(m.last_synced_at,'')
                 AND m2.task_id < m.task_id) )
    )
);
UPDATE remote_mappings
SET filing_repo_id = (SELECT ri.origin_id FROM repo_instances ri WHERE ri.id = remote_mappings.filing_repo_id)
WHERE filing_repo_id != ''
  AND filing_repo_id IN (SELECT id FROM repo_instances);

------------------------------------------------------------------------------
-- 8. Remote-identity index → origin space, keyed on filing_repo_id ALONE. The old
--    COALESCE(filing_repo_id, repo_id) fallback mixed origin ids (filing) with
--    instance ids (logical) post-split; 7a guarantees filing_repo_id is populated
--    for every remote-backed row, so the fallback is unnecessary (RFC 0005 §D4 /
--    resolves Open-Q1: no denormalized logical-origin column needed).
------------------------------------------------------------------------------
DROP INDEX idx_tasks_remote_lookup;
CREATE INDEX idx_tasks_remote_lookup
    ON tasks (filing_repo_id, remote_provider, remote_id)
    WHERE remote_provider IS NOT NULL;
