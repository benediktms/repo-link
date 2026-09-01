-- Indexes backing user-driven task sorting (`rl task list --sort`).
--
-- `TaskFilter::sort` orders by one of `created_at` / `updated_at` / `priority`
-- / `title` / `lifecycle` / `sync_state` / `synced_at`, always with `id` as the
-- tie-breaker, and nearly every call site scopes to one workspace first. The
-- composite indexes below let SQLite satisfy that scope-then-order in index
-- order instead of sorting the workspace's whole task set in memory.
--
-- Only the three time/priority keys get an index: `lifecycle` and `sync_state`
-- already have their own single-column indexes, and `title` sorting is a
-- display convenience whose cost is bounded by the workspace's task count.
CREATE INDEX idx_tasks_ws_created_at ON tasks(workspace_id, created_at, id);
CREATE INDEX idx_tasks_ws_updated_at ON tasks(workspace_id, updated_at, id);
CREATE INDEX idx_tasks_ws_priority   ON tasks(workspace_id, priority, id);
