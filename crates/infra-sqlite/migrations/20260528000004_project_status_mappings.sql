-- RFC 0001 §6 — give the local-status → project-option mapping its own
-- table, fixing the many-to-one regression from Stage 3 (#80).
--
-- Stage 3 flattened the mapping onto a scalar `default_for` column on
-- `project_status_options`: one row per `(project_id, option_id)`, at most
-- one `TaskStatus` per row. But the domain (`Project.status_mappings`)
-- explicitly permits **many statuses → one option** (e.g. `Open` and
-- `Blocked` both → "Backlog"), which that shape simply cannot store — the
-- second mapping was silently dropped on save.
--
-- The fix inverts ownership: mappings live in their own table keyed
-- `(project_id, status)`. That PK *is* the "no duplicate status per
-- project" invariant (previously only enforced in `Project::new`), now
-- mirrored at the DB. `option_id` is not part of the key, so many statuses
-- pointing at one option is the natural case, not a special one.
--
-- sqlx wraps each migration in its own transaction, so we emit no
-- BEGIN/COMMIT and don't toggle PRAGMA foreign_keys here.

-- One row per `(project, status)`. The composite FK keeps every mapping
-- referentially consistent with the option catalog: a mapping can only
-- point at an option this project actually owns, and dropping an option
-- (via the wholesale option-set replace in `save`) cascades its mappings
-- away. The target `(project_id, option_id)` is the PK of
-- `project_status_options`, which satisfies SQLite's requirement that a
-- composite FK reference a unique key.
CREATE TABLE project_status_mappings (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    status     TEXT NOT NULL CHECK (status IN ('open', 'in_progress', 'blocked', 'done')),
    option_id  TEXT NOT NULL,
    PRIMARY KEY (project_id, status),
    FOREIGN KEY (project_id, option_id)
        REFERENCES project_status_options(project_id, option_id) ON DELETE CASCADE
);

-- Lift any existing scalar mappings into the new table. Every selected
-- `(project_id, option_id)` is drawn straight from `project_status_options`,
-- so the composite FK is satisfied by construction. `default_for` already
-- carried the same CHECK domain, so the values are valid `status` values.
INSERT INTO project_status_mappings (project_id, status, option_id)
SELECT project_id, default_for, option_id
  FROM project_status_options
 WHERE default_for IS NOT NULL;

-- `default_for` is now dead. It's a plain column with only a column-level
-- CHECK — not part of any PK or index — so SQLite can drop it directly
-- (3.35+), no table-rebuild dance required.
ALTER TABLE project_status_options DROP COLUMN default_for;
