-- Add a nullable `repo_id` column to `task_snapshots` so `rl task rollback`
-- can restore the task→binding pointer alongside content fields. Without
-- this, rolling back across a `rl task link` / `--relink` leaves the task
-- bound to the post-link repo with the pre-link `remote_id`, an incoherent
-- state with no command path forward.
--
-- Pre-migration snapshots leave the column NULL — rollback to those rows
-- will preserve the task's current binding (the safest fallback; we have no
-- record of what the binding was at the time the historical snapshot was
-- captured). Post-migration snapshots populate the column.
--
-- Strict superset of the previous schema, so existing rows port cleanly.
-- SQLite can't alter a CHECK constraint or add a column with a foreign-key
-- reference in place; uses the rename-copy-drop pattern established by
-- 20260528000001_snapshot_source_add_link.sql.

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
    repo_id          TEXT,
    source           TEXT NOT NULL
                     CHECK (source IN ('created','local_edit','promote','push','pre_pull','pull','conflict_resolve','rollback','link')),
    captured_at      TEXT NOT NULL,
    PRIMARY KEY (task_id, version)
);
INSERT INTO task_snapshots_new (task_id, version, title, body, status, sync_state, priority,
                                assignees_json, remote_provider, remote_id, repo_id, source, captured_at)
SELECT task_id, version, title, body, status, sync_state, priority,
       assignees_json, remote_provider, remote_id, NULL, source, captured_at
FROM task_snapshots;
DROP TABLE task_snapshots;
ALTER TABLE task_snapshots_new RENAME TO task_snapshots;
CREATE INDEX idx_task_snapshots_task   ON task_snapshots(task_id, version DESC);
CREATE INDEX idx_task_snapshots_source ON task_snapshots(task_id, source, version DESC);

PRAGMA foreign_keys = ON;
