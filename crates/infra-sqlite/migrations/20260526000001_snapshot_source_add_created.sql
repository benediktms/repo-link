-- Add `'created'` to the legal set of `task_snapshots.source` values.
--
-- v1 of a freshly-created task is now written with source `'created'`
-- instead of `'local_edit'` (see SnapshotSource::Created in domain-task).
-- The existing CHECK constraint was a strict allowlist, so it must be
-- recreated with the new value — SQLite can't ALTER a CHECK in place.
--
-- This migration uses the same rename-copy-drop pattern as
-- 20260521000001_add_check_constraints_and_fk.sql. The new constraint is
-- a strict superset of the old one, so every existing row passes; no
-- data backfill or repair is required.

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
                     CHECK (source IN ('created','local_edit','promote','push','pre_pull','pull','conflict_resolve','rollback')),
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
