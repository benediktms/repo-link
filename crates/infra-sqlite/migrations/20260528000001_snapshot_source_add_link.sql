-- Add `'link'` to the legal set of `task_snapshots.source` values.
--
-- `rl task link` rewires a task to a different remote (verified relink after a
-- GitHub transfer, or arbitrary attach to an existing remote). The snapshot
-- it writes is tagged `'link'` so the audit history names *why* the remote
-- identity changed instead of folding it into a generic `'local_edit'`.
--
-- Strict superset of the previous allowlist, so no data backfill is needed.
-- SQLite can't ALTER a CHECK in place; uses the same rename-copy-drop pattern
-- as 20260526000001_snapshot_source_add_created.sql.

PRAGMA foreign_keys = OFF;

CREATE TABLE task_snapshots_new (
    task_id          TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    version          INTEGER NOT NULL,
    title            TEXT NOT NULL,
    body             TEXT NOT NULL,
    status           TEXT NOT NULL
                     CHECK (status IN ('open','in_progress','blocked','done','archived')),
    sync_state       TEXT NOT NULL
                     CHECK (sync_state IN ('local_only','staged','synced','dirty_local','dirty_remote','conflict')),
    priority         TEXT NOT NULL
                     CHECK (priority IN ('p0','p1','p2','p3')),
    assignees_json   TEXT NOT NULL,
    remote_provider  TEXT,
    remote_id        TEXT,
    source           TEXT NOT NULL
                     CHECK (source IN ('created','local_edit','promote','push','pre_pull','pull','conflict_resolve','rollback','link')),
    captured_at      TEXT NOT NULL,
    PRIMARY KEY (task_id, version)
);
INSERT INTO task_snapshots_new (task_id, version, title, body, status, sync_state, priority,
                                assignees_json, remote_provider, remote_id, source, captured_at)
SELECT task_id, version, title, body, status, sync_state, priority,
       assignees_json, remote_provider, remote_id, source, captured_at
FROM task_snapshots;
DROP TABLE task_snapshots;
ALTER TABLE task_snapshots_new RENAME TO task_snapshots;
CREATE INDEX idx_task_snapshots_task   ON task_snapshots(task_id, version DESC);
CREATE INDEX idx_task_snapshots_source ON task_snapshots(task_id, source, version DESC);

PRAGMA foreign_keys = ON;
