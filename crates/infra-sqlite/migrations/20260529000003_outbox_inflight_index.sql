-- RFC 0001 Stage 6 (#54) — index the per-task-FIFO claim's sibling lookup.
--
-- `claim_next_eligible` filters candidates with a correlated subquery keyed on
-- `task_id` + status (an `inflight` entry, or an older `pending` one, blocks
-- the candidate). The existing `idx_outbox_pending(status, enqueued_at)
-- WHERE status='pending'` serves the outer ORDER-BY but not that per-task
-- sibling scan, which would otherwise re-scan the table per candidate. The
-- table is append-only (succeeded/failed rows accumulate over the tool's
-- lifetime), so add a partial index on `task_id` over the non-terminal rows
-- the subquery actually touches. Additive; no backfill.
CREATE INDEX idx_outbox_task_active ON outbox_entries(task_id, enqueued_at)
    WHERE status IN ('pending', 'inflight');
