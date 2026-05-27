-- Friendly task IDs: each task carries a short globally-unique `hash` and
-- each repo carries a short globally-unique `prefix`. Together they form
-- the composite `prefix-hash` display ID (e.g. `rlk-ak7`) that replaces
-- the raw UUID in JSON output. UUIDs remain the on-disk primary key.
--
-- Both columns default to '' (empty string sentinel) so legacy rows can
-- live through the migration; an `open_db`-time Rust backfill populates
-- them. The partial UNIQUE indexes treat empty strings as "not yet set"
-- and only enforce uniqueness across populated values — matching the
-- pattern established for `repos.name` in the 20260521 migration.

ALTER TABLE repos ADD COLUMN prefix TEXT NOT NULL DEFAULT '';
ALTER TABLE tasks ADD COLUMN hash   TEXT NOT NULL DEFAULT '';

CREATE UNIQUE INDEX idx_repos_prefix ON repos(prefix) WHERE prefix != '';
CREATE UNIQUE INDEX idx_tasks_hash   ON tasks(hash)   WHERE hash   != '';
