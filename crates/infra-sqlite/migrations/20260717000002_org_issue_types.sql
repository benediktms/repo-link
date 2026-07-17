-- RFC 0006 D5/D8 — org-scoped native issue-type registry cache.
--
-- GitHub's native issue types are an ORGANIZATION-level catalog shared across
-- every repo/project the org owns, so this cache is keyed on the owner login
-- and decoupled from any board (D5) — deliberately NO FK to `projects`.
--
-- ADDITIVE new table only — no ALTER of any existing table, so it is safe under
-- the sqlx-sqlite forced-txn / no-parent-rebuild constraint. Replace-wholesale
-- on (re)fetch (a cache, not a source of truth), so no created_at/updated_at.
--
-- The PK is (owner_login, issue_type_id): the id is the stable per-org
-- disambiguator, and `save` clears + re-inserts an owner's rows as a unit. An
-- absent owner is simply zero rows — the D8 "type unavailable" signal — not an
-- error.

CREATE TABLE org_issue_types (
    owner_login     TEXT NOT NULL,
    issue_type_id   TEXT NOT NULL,
    name            TEXT NOT NULL,
    PRIMARY KEY (owner_login, issue_type_id)
);
