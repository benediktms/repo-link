-- RFC 0006 D2 (project-side) + D9 — generalize the project field model.
--
-- Historically a project stored exactly ONE single-select field: the Status
-- field, as `projects.status_field_id` + the `project_status_options` catalog.
-- The generalized model keeps EVERY retained single-select field as a keyed
-- set: `project_fields` (one row per field, tagged with a `kind`) and
-- `project_field_options` (that field's option catalog). The existing Status
-- field + options migrate into them; `project_status_mappings` re-points its
-- composite FK at the generalized options.
--
-- This is additive + a single LEAF rebuild. sqlx wraps each migration in its
-- own transaction, so we emit no BEGIN/COMMIT and don't toggle PRAGMA
-- foreign_keys (it is a no-op inside the forced txn). Correctness relies on the
-- child-first ordering below (rebuild the mappings leaf onto the new options
-- BEFORE dropping the old options table), not on toggling FKs.

-- 1. New generalized field tables (additive).
CREATE TABLE project_fields (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    field_id   TEXT NOT NULL,
    name       TEXT NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('status', 'priority', 'other')),
    PRIMARY KEY (project_id, field_id)
);

-- Options catalog per field. The composite PK is the unique key the mappings'
-- FK below references. ON DELETE CASCADE off `project_fields` clears a field's
-- options when the field goes away (and transitively when the project does).
CREATE TABLE project_field_options (
    project_id TEXT NOT NULL,
    field_id   TEXT NOT NULL,
    option_id  TEXT NOT NULL,
    name       TEXT NOT NULL,
    ordinal    INTEGER NOT NULL,
    PRIMARY KEY (project_id, field_id, option_id),
    FOREIGN KEY (project_id, field_id)
        REFERENCES project_fields(project_id, field_id) ON DELETE CASCADE
);

-- 2. Backfill the existing Status field + its options. The field NAME was never
--    stored (only `status_field_id`), so the migrated field is labelled
--    'Status' — functionally harmless, since it is identified by kind='status'
--    on load, not by name. A later re-link/fetch overwrites the name.
INSERT INTO project_fields (project_id, field_id, name, kind)
SELECT id, status_field_id, 'Status', 'status' FROM projects;

INSERT INTO project_field_options (project_id, field_id, option_id, name, ordinal)
SELECT o.project_id, p.status_field_id, o.option_id, o.name, o.ordinal
  FROM project_status_options o
  JOIN projects p ON p.id = o.project_id;

-- 3. Rebuild the LEAF `project_status_mappings`: add `field_id` and re-point its
--    composite FK at `project_field_options`. Nothing FKs this table, so the
--    rename-copy-DROP is safe (precedent: 20260622000002). The transitive
--    cascade project → project_fields → project_field_options → this table
--    replaces the old direct projects FK.
CREATE TABLE project_status_mappings_new (
    project_id TEXT NOT NULL,
    field_id   TEXT NOT NULL,
    is_open    INTEGER NOT NULL CHECK (is_open IN (0, 1)),
    option_id  TEXT NOT NULL,
    PRIMARY KEY (project_id, is_open),
    FOREIGN KEY (project_id, field_id, option_id)
        REFERENCES project_field_options(project_id, field_id, option_id) ON DELETE CASCADE
);
INSERT INTO project_status_mappings_new (project_id, field_id, is_open, option_id)
SELECT m.project_id, p.status_field_id, m.is_open, m.option_id
  FROM project_status_mappings m
  JOIN projects p ON p.id = m.project_id;
DROP TABLE project_status_mappings;
ALTER TABLE project_status_mappings_new RENAME TO project_status_mappings;

-- 4. Drop the now-orphan options table (leaf after step 3 re-pointed the
--    mappings) and the now-redundant Status-field pointer column. The column is
--    a plain non-key column, so SQLite 3.35+ does a direct DROP COLUMN — no
--    table rebuild, and child FKs onto projects(id) stay valid (precedent:
--    20260528000004 dropped project_status_options.default_for).
DROP TABLE project_status_options;
ALTER TABLE projects DROP COLUMN status_field_id;
