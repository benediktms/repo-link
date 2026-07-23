ALTER TABLE tasks
ADD COLUMN issue_type_pending INTEGER NOT NULL DEFAULT 0
CHECK (issue_type_pending IN (0, 1));
