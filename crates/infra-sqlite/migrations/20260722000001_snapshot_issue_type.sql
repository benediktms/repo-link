ALTER TABLE task_snapshots ADD COLUMN issue_type TEXT;

ALTER TABLE task_snapshots
ADD COLUMN issue_type_recorded INTEGER NOT NULL DEFAULT 0
CHECK (issue_type_recorded IN (0, 1));
