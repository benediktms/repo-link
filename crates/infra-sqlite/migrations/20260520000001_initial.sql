-- Initial schema for repo-link local SQLite store.

CREATE TABLE workspaces (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    description   TEXT,
    status        TEXT NOT NULL,
    local_only    INTEGER NOT NULL,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);

CREATE TABLE repos (
    id              TEXT PRIMARY KEY,
    workspace_id    TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    remote_url      TEXT NOT NULL,
    canonical_url   TEXT NOT NULL,
    tracked_branch  TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    UNIQUE(workspace_id, canonical_url)
);
CREATE INDEX idx_repos_workspace ON repos(workspace_id);

CREATE TABLE worktree_links (
    repo_id        TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    path           TEXT NOT NULL,
    branch         TEXT,
    status         TEXT NOT NULL,
    last_seen_at   TEXT NOT NULL,
    PRIMARY KEY (repo_id, path)
);
CREATE INDEX idx_worktrees_status ON worktree_links(status);

CREATE TABLE tasks (
    id               TEXT PRIMARY KEY,
    workspace_id     TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    repo_id          TEXT REFERENCES repos(id) ON DELETE SET NULL,
    title            TEXT NOT NULL,
    body             TEXT NOT NULL,
    -- Lifecycle: 'open' | 'in_progress' | 'blocked' | 'done' | 'archived'.
    status           TEXT NOT NULL,
    -- Sync: 'local_only' | 'staged' | 'synced' | 'dirty_local' | 'dirty_remote' | 'conflict'.
    sync_state       TEXT NOT NULL,
    priority         TEXT NOT NULL,
    assignees_json   TEXT NOT NULL DEFAULT '[]',
    remote_provider  TEXT,
    remote_id        TEXT,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL
);
CREATE INDEX idx_tasks_workspace  ON tasks(workspace_id);
CREATE INDEX idx_tasks_status     ON tasks(status);
CREATE INDEX idx_tasks_sync_state ON tasks(sync_state);
CREATE INDEX idx_tasks_repo       ON tasks(repo_id);

CREATE TABLE task_relations (
    task_id        TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL,
    other_task_id  TEXT NOT NULL,
    PRIMARY KEY (task_id, kind, other_task_id)
);
CREATE INDEX idx_task_relations_other ON task_relations(other_task_id);

-- task_snapshots: append-only event log of task state. Every save writes a
-- new row. The `tasks` row above is the current projection (denormalized
-- for fast reads); this table is the authoritative history.
--
-- `source` distinguishes who triggered the snapshot, which matters for
-- diff-baseline selection: only snapshots written by remote-aligning
-- events ('promote' / 'push' / 'pull' / 'conflict_resolve') count as the
-- "last known remote state" against which dirty detection runs. Local
-- edits and pre-pull captures intersperse the baseline without resetting
-- it. `rollback` snapshots audit the rollback itself.
CREATE TABLE task_snapshots (
    task_id          TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    version          INTEGER NOT NULL,
    title            TEXT NOT NULL,
    body             TEXT NOT NULL,
    status           TEXT NOT NULL,
    sync_state       TEXT NOT NULL,
    priority         TEXT NOT NULL,
    assignees_json   TEXT NOT NULL,
    remote_provider  TEXT,
    remote_id        TEXT,
    -- 'local_edit' | 'promote' | 'push' | 'pre_pull' | 'pull' |
    -- 'conflict_resolve' | 'rollback'
    source           TEXT NOT NULL,
    captured_at      TEXT NOT NULL,
    PRIMARY KEY (task_id, version)
);
CREATE INDEX idx_task_snapshots_task ON task_snapshots(task_id, version DESC);
CREATE INDEX idx_task_snapshots_source ON task_snapshots(task_id, source, version DESC);

CREATE TABLE remote_mappings (
    task_id                 TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    provider                TEXT NOT NULL,
    remote_id               TEXT NOT NULL,
    last_remote_updated_at  TEXT,
    last_synced_at          TEXT,
    UNIQUE(provider, remote_id)
);

CREATE TABLE sync_events (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    at            TEXT NOT NULL,
    workspace_id  TEXT,
    payload_json  TEXT NOT NULL
);
CREATE INDEX idx_sync_events_at ON sync_events(at);

CREATE TABLE audit_log (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    at            TEXT NOT NULL,
    kind          TEXT NOT NULL,
    payload_json  TEXT NOT NULL
);
CREATE INDEX idx_audit_log_at ON audit_log(at);
