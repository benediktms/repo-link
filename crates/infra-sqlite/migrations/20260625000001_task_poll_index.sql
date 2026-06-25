-- RFC 0004 Phase 4 — index backing the poller's per-tick stale-scan.
--
-- The poller selects project-backed tasks in active workspaces whose `synced_at`
-- is NULL or stale, ordered oldest-first, capped by LIMIT (see
-- `TaskFilter { has_project_item_id, synced_at_lt, active_workspaces_only, limit }`).
-- The partial index covers exactly that working set (project-backed rows) and
-- carries `synced_at` last so the ORDER BY is index-ordered — keeping the
-- per-tick cost O(log N + stale_count) rather than O(N).
CREATE INDEX idx_tasks_poll
    ON tasks(workspace_id, project_item_id, synced_at)
    WHERE project_item_id IS NOT NULL;
