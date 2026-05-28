-- RFC 0001 Stage 3 — storage for the project-sync axis.
--
-- Adds four schema changes, all purely additive against existing rows:
--   * `projects`              — local mirror of a GitHub Projects v2 board.
--   * `project_status_options`— per-project status field options + the
--                               local-TaskStatus → option default mapping.
--   * `workspaces.project_id` — optional parent project (RFC §3 D1).
--   * `tasks.project_item_id` — per-task GitHub project item node ID
--                               (`PVTI_…`), set once a mirror task is
--                               attached to a project. Partial index keyed
--                               on non-NULL so projectless tasks are cheap.
--   * `tasks.remote_node_id`  — issue node ID (`I_…`) captured alongside
--                               the existing per-repo number, so GraphQL
--                               mutations don't have to translate from the
--                               number on the hot path.
--   * `outbox_entries`        — queue of pending outbound mutations for
--                               mirror tasks. The drainer (Stage 6) pops
--                               oldest-pending in a transaction.
--
-- Existing rows migrate cleanly: new columns are nullable / default-zero;
-- no backfill needed. Nothing reads these yet — Stage 4 wires the
-- `application-project` service that puts data in them.

-- Projects are workspace-independent: one project can parent many
-- workspaces. The PK `id` IS the GitHub node ID (`PVT_…`) — projects are
-- a 100% mirror of the remote entity, so we don't mint a separate UUID.
CREATE TABLE projects (
    id              TEXT PRIMARY KEY,
    provider        TEXT NOT NULL CHECK (provider IN ('github')),
    owner_login     TEXT NOT NULL,
    number          INTEGER NOT NULL,
    title           TEXT NOT NULL,
    status_field_id TEXT NOT NULL,
    archived        INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

-- One row per option on the project's single-select Status field plus the
-- `default_for` slot that pins which local TaskStatus this option is the
-- default for (NULL = the option exists on the board but isn't anyone's
-- default — totally legitimate when the user's board has options we don't
-- recognise, e.g. "Triage" or "In Review"). The domain validator
-- (`Project::new`) enforces that within one project, no `default_for`
-- value appears twice; we don't lock that at the DB layer because the
-- shape of the remote board is user-defined, and reading the row back
-- through `Project::new` is sufficient to catch a corrupted state.
CREATE TABLE project_status_options (
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    option_id   TEXT NOT NULL,
    name        TEXT NOT NULL,
    default_for TEXT CHECK (default_for IN ('open', 'in_progress', 'blocked', 'done')),
    ordinal     INTEGER NOT NULL,
    PRIMARY KEY (project_id, option_id)
);

-- Optional parent project. Existing workspaces migrate with NULL — that's
-- the local-only / projectless path and stays valid forever.
ALTER TABLE workspaces ADD COLUMN project_id TEXT
    REFERENCES projects(id) ON DELETE SET NULL;

-- Project item node ID, set once a mirror task gets attached to a project.
-- The partial index keeps the projectless majority of rows out of the
-- B-tree so polling-by-item-id stays fast as task count grows.
ALTER TABLE tasks ADD COLUMN project_item_id TEXT;
CREATE INDEX idx_tasks_project_item_id ON tasks(project_item_id)
    WHERE project_item_id IS NOT NULL;

-- Issue node ID stored alongside the per-repo `remote_id` number. Both
-- identities are persisted so we never translate one to the other on the
-- hot path: REST endpoints address by number, GraphQL mutations address
-- by node id, and a synced issue has both from its first promote.
ALTER TABLE tasks ADD COLUMN remote_node_id TEXT;

-- One row per pending outbound mutation against a mirror task. `mutation_kind`
-- is the discriminator from `domain_sync::OutboxMutation::kind()`; the
-- serialized payload lives in `payload_json`. `status` walks
-- pending → inflight → succeeded / failed. The partial index on the
-- pending slice keeps the drainer's `next_pending` lookup O(log N) of the
-- pending count, not of the lifetime row count.
CREATE TABLE outbox_entries (
    id            TEXT PRIMARY KEY,
    task_id       TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    mutation_kind TEXT NOT NULL,
    payload_json  TEXT NOT NULL,
    status        TEXT NOT NULL CHECK (status IN ('pending', 'inflight', 'succeeded', 'failed')),
    attempts      INTEGER NOT NULL DEFAULT 0,
    last_error    TEXT,
    enqueued_at   TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE INDEX idx_outbox_pending ON outbox_entries(status, enqueued_at)
    WHERE status = 'pending';
