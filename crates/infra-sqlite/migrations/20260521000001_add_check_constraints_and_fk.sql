-- Add domain CHECK constraints to enum/boolean columns and an FK on
-- task_relations.other_task_id. SQLite does not support
-- `ALTER TABLE ... ADD CONSTRAINT`, so each affected table is recreated
-- via the standard `_new` rename dance. All existing indexes are
-- re-created against the renamed tables.
--
-- sqlx wraps each migration in its own transaction, so we do not emit
-- BEGIN/COMMIT or toggle PRAGMA foreign_keys here. The rebuilds use
-- INSERT...SELECT and the renamed tables keep their original IDs, so
-- existing FK rows remain pointing at valid targets across the swap.

-- workspaces: CHECK status, CHECK local_only is boolean (0/1).
CREATE TABLE workspaces_new (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    description   TEXT,
    status        TEXT NOT NULL
                  CHECK (status IN ('created','active','paused','archived','deleted')),
    local_only    INTEGER NOT NULL
                  CHECK (local_only IN (0,1)),
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
INSERT INTO workspaces_new (id, name, description, status, local_only, created_at, updated_at)
SELECT id, name, description, status, local_only, created_at, updated_at
FROM workspaces;
DROP TABLE workspaces;
ALTER TABLE workspaces_new RENAME TO workspaces;

-- tasks: CHECK status, sync_state, priority. Preserve all FKs and indexes.
CREATE TABLE tasks_new (
    id               TEXT PRIMARY KEY,
    workspace_id     TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    repo_id          TEXT REFERENCES repos(id) ON DELETE SET NULL,
    title            TEXT NOT NULL,
    body             TEXT NOT NULL,
    status           TEXT NOT NULL
                     CHECK (status IN ('open','in_progress','blocked','done','archived')),
    sync_state       TEXT NOT NULL
                     CHECK (sync_state IN ('local_only','staged','synced','dirty_local','dirty_remote','conflict')),
    priority         TEXT NOT NULL
                     CHECK (priority IN ('p0','p1','p2','p3')),
    assignees_json   TEXT NOT NULL DEFAULT '[]',
    remote_provider  TEXT,
    remote_id        TEXT,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL
);
INSERT INTO tasks_new (id, workspace_id, repo_id, title, body, status, sync_state, priority,
                       assignees_json, remote_provider, remote_id, created_at, updated_at)
SELECT id, workspace_id, repo_id, title, body, status, sync_state, priority,
       assignees_json, remote_provider, remote_id, created_at, updated_at
FROM tasks;
DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;
CREATE INDEX idx_tasks_workspace  ON tasks(workspace_id);
CREATE INDEX idx_tasks_status     ON tasks(status);
CREATE INDEX idx_tasks_sync_state ON tasks(sync_state);
CREATE INDEX idx_tasks_repo       ON tasks(repo_id);

-- task_relations: add FK on other_task_id → tasks(id) ON DELETE CASCADE.
CREATE TABLE task_relations_new (
    task_id        TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL,
    other_task_id  TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    PRIMARY KEY (task_id, kind, other_task_id)
);
INSERT INTO task_relations_new (task_id, kind, other_task_id)
SELECT task_id, kind, other_task_id
FROM task_relations;
DROP TABLE task_relations;
ALTER TABLE task_relations_new RENAME TO task_relations;
CREATE INDEX idx_task_relations_other ON task_relations(other_task_id);

-- task_snapshots: CHECK status, sync_state, priority, source.
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
                     CHECK (source IN ('local_edit','promote','push','pre_pull','pull','conflict_resolve','rollback')),
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
