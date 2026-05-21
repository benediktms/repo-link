ALTER TABLE repos ADD COLUMN name TEXT NOT NULL DEFAULT '';
ALTER TABLE repos ADD COLUMN aliases TEXT NOT NULL DEFAULT '[]'
    CHECK (json_valid(aliases) AND json_type(aliases) = 'array');
