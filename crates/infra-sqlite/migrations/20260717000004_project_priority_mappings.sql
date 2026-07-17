-- RFC 0006 D3 — per-project local-priority → board-option mapping.
--
-- Sibling of `project_status_mappings` (its post-generalization shape,
-- 20260717000001), but keyed on the local `Priority` enum (`p0..p3`) instead of
-- the open/closed bit. The rows are DERIVED at `rl project link` by ordinal
-- (`derive_priority_mappings`) and overridable by hand — a cache, not the source
-- of truth. The outbound projection that reads them to set a board's Priority
-- single-select is a follow-up (#225); this migration only stores them.
--
-- Additive: one new leaf table, no parent rebuild (respecting the sqlx-sqlite
-- constraint). The composite FK targets `project_field_options(project_id,
-- field_id, option_id)` — the same generalized catalog the Status mapping
-- references — so a priority mapping can only point at an option the board
-- actually owns, and dropping the field/option (or, transitively, the project)
-- cascades the mapping away. sqlx wraps each migration in its own transaction,
-- so no BEGIN/COMMIT and no PRAGMA foreign_keys toggle here.
CREATE TABLE project_priority_mappings (
    project_id TEXT NOT NULL,
    field_id   TEXT NOT NULL,
    priority   TEXT NOT NULL CHECK (priority IN ('p0', 'p1', 'p2', 'p3')),
    option_id  TEXT NOT NULL,
    PRIMARY KEY (project_id, priority),
    FOREIGN KEY (project_id, field_id, option_id)
        REFERENCES project_field_options(project_id, field_id, option_id) ON DELETE CASCADE
);
