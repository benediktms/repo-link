-- RFC 0004 Phase 1 — re-key the local lifecycle → project-option mapping on the
-- open/closed bit (`is_open`) instead of the now-deleted 5-state `TaskStatus`.
--
-- The lifecycle collapsed to open/closed (RFC 0004 D1), so a project has at most
-- two mapping rows: one for open tasks, one for closed. The old PK was
-- `(project_id, status)` with `status IN (open,in_progress,blocked,done)`.
--
-- `project_status_mappings` is a LEAF child (it FKs `project_status_options`;
-- nothing references it), so the rename-copy-DROP rebuild is safe — DROPping it
-- cannot cascade-delete anything. Same dance as the snapshot-source migrations.
-- Daemon note: stop the daemon or wait for its tick before applying, so the
-- forced-transaction pool is uncontended.
--
-- Backfill collapses the old buckets: `done` → closed (is_open=0); everything
-- else (open/in_progress/blocked) → open (is_open=1). Old projects may have had
-- several open-ish rows (e.g. both Open and Blocked mapped) — `INSERT OR IGNORE`
-- keeps the first per `(project_id, is_open)` bucket so the new PK holds.

PRAGMA foreign_keys = OFF;

CREATE TABLE project_status_mappings_new (
    project_id TEXT NOT NULL,
    is_open    INTEGER NOT NULL CHECK (is_open IN (0, 1)),
    option_id  TEXT NOT NULL,
    PRIMARY KEY (project_id, is_open),
    FOREIGN KEY (project_id, option_id)
        REFERENCES project_status_options(project_id, option_id) ON DELETE CASCADE
);
-- When several open-ish rows (open/in_progress/blocked) collapse into the one
-- is_open=1 bucket, `INSERT OR IGNORE` keeps the FIRST per (project_id,
-- is_open). Order so the literal `open` mapping wins (then in_progress, then
-- blocked) — a plain `ORDER BY status` would pick 'blocked' alphabetically and
-- point newly-open tasks at the wrong board column.
INSERT OR IGNORE INTO project_status_mappings_new (project_id, is_open, option_id)
SELECT project_id,
       CASE status WHEN 'done' THEN 0 ELSE 1 END AS is_open,
       option_id
FROM project_status_mappings
ORDER BY project_id,
         CASE status
             WHEN 'done' THEN 0
             WHEN 'open' THEN 0
             WHEN 'in_progress' THEN 1
             WHEN 'blocked' THEN 2
             ELSE 3
         END;
DROP TABLE project_status_mappings;
ALTER TABLE project_status_mappings_new RENAME TO project_status_mappings;

PRAGMA foreign_keys = ON;
