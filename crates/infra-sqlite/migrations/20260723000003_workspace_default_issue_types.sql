-- RFC 0006 #239 / §0 A4 — workspace default native issue-type names.
--
-- When a task is first filed (`sync promote`) with no explicit issue type, the
-- effective type is derived from these workspace-scoped defaults: a `child_of`
-- (sub-issue) task uses `default_sub_issue_type`, a free-standing task uses
-- `default_issue_type`. Stored as the configured NAME (org-specific, e.g.
-- "Story" / "Task") — resolved case-insensitively against the filing owner's
-- live `org_issue_types` registry at projection time, never hardcoded.
--
-- Plain additive ADD COLUMN, NULLABLE, un-backfilled — the house rule for the
-- `workspaces` parent table (a rename-copy-drop rebuild would cascade-delete
-- children under sqlx's forced txn; see 20260530000001). Both ship NULL = "no
-- default", so behaviour is unchanged for every existing workspace. Any new
-- column must also join the `WORKSPACE_COLS` const (schema-consistency
-- contract, #110).
ALTER TABLE workspaces ADD COLUMN default_issue_type TEXT;
ALTER TABLE workspaces ADD COLUMN default_sub_issue_type TEXT;
