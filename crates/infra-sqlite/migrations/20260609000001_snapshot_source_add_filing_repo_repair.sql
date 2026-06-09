-- Add `'filing_repo_repair'` to the legal set of `task_snapshots.source`
-- values (rpl-sv2 / RFC 0002 D2 doctor repair).
--
-- `rl repo doctor --repair` (and the application-task service method
-- `TaskService::repoint_filing_repo` that it calls) re-points a task's
-- recorded `filing_repo_id` to a live binding. The snapshot it writes
-- is tagged `'filing_repo_repair'` so the audit history names *why*
-- the filing column changed instead of folding it into a generic
-- `'local_edit'` — and so a future `task_snapshots.source` tripwire
-- (the canonical mirror field set in `domain-task`) catches every
-- doctor re-point with a single grep.
--
-- Baseline-eligible on purpose: the doctor re-point is an
-- *authoritative* user action (the recorded `filing_repo_id` was
-- dangling; the new value is correct and live), so the next
-- `sync pull` must NOT fire a phantom drift on the new canonical
-- (that would be exactly the silent-divergence shape rpl-sv2 is
-- here to heal). See `domain_task::SnapshotSource::FilingRepoRepair`
-- for the matching Rust enum variant + `is_baseline` membership.
--
-- Strict superset of the prior allowlist, so no data backfill is
-- needed. SQLite can't ALTER a CHECK in place; uses the same
-- rename-copy-drop pattern as the prior `link` / `repo_id` /
-- `filing_repo_id` migrations. The full column list below MUST match
-- the latest `task_snapshots` schema — divergence trips the
-- `_COLS`-const tripwire (#110) at the next snapshot roundtrip.
-- Daemon note: stop the daemon before applying, or wait for its next
-- 45-second tick to complete, so the migration's forced-transaction
-- pool is uncontended. (The pattern is established; see the prior
-- snapshot-source migrations for the same dance.)

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
    repo_id_recorded INTEGER NOT NULL DEFAULT 0,
    filing_repo_id   TEXT,
    source           TEXT NOT NULL
                     CHECK (source IN ('created','local_edit','promote','push','pre_pull','pull','conflict_resolve','rollback','link','filing_repo_repair')),
    captured_at      TEXT NOT NULL,
    PRIMARY KEY (task_id, version)
);
INSERT INTO task_snapshots_new (task_id, version, title, body, status, sync_state, priority,
                                assignees_json, remote_provider, remote_id, repo_id, repo_id_recorded,
                                filing_repo_id, source, captured_at)
SELECT task_id, version, title, body, status, sync_state, priority,
       assignees_json, remote_provider, remote_id, repo_id, repo_id_recorded,
       filing_repo_id, source, captured_at
FROM task_snapshots;
DROP TABLE task_snapshots;
ALTER TABLE task_snapshots_new RENAME TO task_snapshots;
CREATE INDEX idx_task_snapshots_task   ON task_snapshots(task_id, version DESC);
CREATE INDEX idx_task_snapshots_source ON task_snapshots(task_id, source, version DESC);

PRAGMA foreign_keys = ON;
