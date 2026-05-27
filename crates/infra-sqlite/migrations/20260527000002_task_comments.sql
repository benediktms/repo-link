-- Mirror GitHub issue comments into local tasks (append-only).
--
-- `id` is a local surrogate so a comment has a stable identity even before it
-- has a remote id (the outbound/pending path is a follow-up). `remote_comment_id`
-- is the `NOT NULL DEFAULT ''` sentinel: mirrored comments carry the GitHub id,
-- pending local comments use ''. The partial unique index dedupes mirrored
-- comments per task while letting multiple pending comments coexist.

CREATE TABLE task_comments (
    id                  TEXT PRIMARY KEY,
    task_id             TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    remote_comment_id   TEXT NOT NULL DEFAULT '',
    author              TEXT NOT NULL,
    body                TEXT NOT NULL,
    created_at          TEXT NOT NULL
);

CREATE INDEX idx_task_comments_task ON task_comments(task_id);

CREATE UNIQUE INDEX uq_task_comments_remote
    ON task_comments(task_id, remote_comment_id)
    WHERE remote_comment_id != '';
