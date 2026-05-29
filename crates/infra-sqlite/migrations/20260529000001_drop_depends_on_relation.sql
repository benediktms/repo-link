-- Fold `depends_on` task relations into `blocked_by`.
--
-- `depends_on` was one of the relation kinds the domain enum modelled but
-- never wired into any behaviour. It is directionally identical to
-- `blocked_by` ("the other task must land first"), so it was dropped from
-- `RelationKind` as a redundant synonym. Any rows created via
-- `rl task relate --kind depends_on` would now fail to deserialize on load,
-- so fold them into the surviving `blocked_by` kind to preserve intent.
--
-- `task_relations.kind` is free-text (no CHECK constraint), so no table
-- rebuild is needed — only a data rewrite. PRIMARY KEY is
-- (task_id, kind, other_task_id): `UPDATE OR IGNORE` skips any row whose
-- target `blocked_by` edge already exists, and the trailing DELETE clears
-- those now-redundant `depends_on` leftovers.

UPDATE OR IGNORE task_relations
SET kind = 'blocked_by'
WHERE kind = 'depends_on';

DELETE FROM task_relations
WHERE kind = 'depends_on';
