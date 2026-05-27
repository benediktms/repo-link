-- Repo-scope the remote-issue identity.
--
-- GitHub issue numbers are per-repo, so (provider, remote_id) is not globally
-- unique: two repos in one workspace can both have issue #123. The old
-- UNIQUE(provider, remote_id) collided them, breaking `promote` (and any
-- remote lookup) across repos. Including repo_id in the key fixes that.
--
-- SQLite can't drop a table-level UNIQUE in place, so rebuild the table and
-- backfill repo_id from the owning task (synced tasks always carry a repo).

-- repo_id is `NOT NULL DEFAULT ''` rather than nullable: it's part of the
-- UNIQUE key, and SQLite treats NULLs as distinct, so a nullable column would
-- let duplicates slip past the constraint. The empty-string sentinel (matching
-- the convention used for tasks.hash / repos.prefix) keeps the key well-defined
-- even for the degenerate case of a remote-backed task with no repo binding
-- (allowed at the storage layer, though such a task can't actually sync).
CREATE TABLE remote_mappings_new (
    task_id                 TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    repo_id                 TEXT NOT NULL DEFAULT '',
    provider                TEXT NOT NULL,
    remote_id               TEXT NOT NULL,
    last_remote_updated_at  TEXT,
    last_synced_at          TEXT,
    UNIQUE(repo_id, provider, remote_id)
);

INSERT INTO remote_mappings_new
    (task_id, repo_id, provider, remote_id, last_remote_updated_at, last_synced_at)
SELECT m.task_id, COALESCE(t.repo_id, ''), m.provider, m.remote_id, m.last_remote_updated_at, m.last_synced_at
FROM remote_mappings m
JOIN tasks t ON t.id = m.task_id;

DROP TABLE remote_mappings;
ALTER TABLE remote_mappings_new RENAME TO remote_mappings;
