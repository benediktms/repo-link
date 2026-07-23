CREATE INDEX idx_outbox_set_issue_type_dedupe
    ON outbox_entries(task_id, mutation_kind, status, payload_json);
