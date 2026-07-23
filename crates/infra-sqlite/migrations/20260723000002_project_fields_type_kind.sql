-- RFC 0006 #238 — widen the `project_fields.kind` CHECK to admit 'type', so a
-- custom Projects v2 "Type"/"Types" single-select can be classified + persisted
-- (ProjectFieldKind::Type) alongside 'status'/'priority'/'other'.
--
-- SQLite has no ALTER ... DROP/ALTER CONSTRAINT, so relaxing a CHECK means
-- rebuilding the table. `project_fields` is a PARENT (project_field_options
-- FKs it; the two mapping tables FK the options), and sqlx wraps each migration
-- in a forced transaction where `PRAGMA foreign_keys=OFF` is a no-op — so a
-- naive `DROP TABLE project_fields` would fire ON DELETE CASCADE and wipe the
-- options + mappings (the parent-rebuild trap this project has hit before).
--
-- Instead: stash every table's rows into FK-free TEMP tables, drop the real
-- tables child-first (so dropping the parent finds no children to cascade
-- into), recreate them parent-first with FINAL names (only `project_fields`
-- changes — its widened CHECK), and restore parent-first. Recreating with the
-- final names avoids relying on RENAME rewriting child FK references. The three
-- unchanged tables are recreated byte-for-byte identical to their current
-- definitions (20260717000001 + 20260717000004).

-- 1. Stash. TEMP tables carry no constraints, so this is a pure data copy.
CREATE TEMP TABLE _pf  AS SELECT * FROM project_fields;
CREATE TEMP TABLE _pfo AS SELECT * FROM project_field_options;
CREATE TEMP TABLE _psm AS SELECT * FROM project_status_mappings;
CREATE TEMP TABLE _ppm AS SELECT * FROM project_priority_mappings;

-- 2. Drop child-first: each drop's implicit row-delete has no surviving child
--    to cascade into, so no data beyond the (already-stashed) table is lost.
DROP TABLE project_status_mappings;
DROP TABLE project_priority_mappings;
DROP TABLE project_field_options;
DROP TABLE project_fields;

-- 3. Recreate parent-first. Only `project_fields` differs from before: the
--    CHECK now includes 'type'.
CREATE TABLE project_fields (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    field_id   TEXT NOT NULL,
    name       TEXT NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('status', 'priority', 'type', 'other')),
    PRIMARY KEY (project_id, field_id)
);

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

CREATE TABLE project_status_mappings (
    project_id TEXT NOT NULL,
    field_id   TEXT NOT NULL,
    is_open    INTEGER NOT NULL CHECK (is_open IN (0, 1)),
    option_id  TEXT NOT NULL,
    PRIMARY KEY (project_id, is_open),
    FOREIGN KEY (project_id, field_id, option_id)
        REFERENCES project_field_options(project_id, field_id, option_id) ON DELETE CASCADE
);

CREATE TABLE project_priority_mappings (
    project_id TEXT NOT NULL,
    field_id   TEXT NOT NULL,
    priority   TEXT NOT NULL CHECK (priority IN ('p0', 'p1', 'p2', 'p3')),
    option_id  TEXT NOT NULL,
    PRIMARY KEY (project_id, priority),
    FOREIGN KEY (project_id, field_id, option_id)
        REFERENCES project_field_options(project_id, field_id, option_id) ON DELETE CASCADE
);

-- 4. Restore parent-first (FK-safe) and drop the stashes.
INSERT INTO project_fields           SELECT * FROM _pf;
INSERT INTO project_field_options    SELECT * FROM _pfo;
INSERT INTO project_status_mappings  SELECT * FROM _psm;
INSERT INTO project_priority_mappings SELECT * FROM _ppm;

DROP TABLE _pf;
DROP TABLE _pfo;
DROP TABLE _psm;
DROP TABLE _ppm;
