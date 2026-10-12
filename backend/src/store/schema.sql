CREATE TABLE audits (
    id TEXT COLLATE BINARY PRIMARY KEY NOT NULL,
    key_id TEXT COLLATE BINARY NOT NULL,
    first_observed_at INTEGER NOT NULL,
    request_id TEXT,
    thread_id TEXT,
    session_id TEXT,
    turn_id TEXT,
    input_kinds TEXT NOT NULL CHECK (json_type(input_kinds) = 'array'),
    repositories TEXT CHECK (repositories IS NULL OR json_type(repositories) = 'array'),
    input_content_id TEXT COLLATE BINARY NOT NULL,
    input_text TEXT NOT NULL,
    input_search TEXT NOT NULL,
    preview TEXT NOT NULL,
    context_count INTEGER NOT NULL CHECK (context_count >= 0)
) STRICT;

CREATE INDEX audit_time ON audits (first_observed_at DESC, id DESC);
CREATE INDEX audit_key_time ON audits (key_id, first_observed_at DESC, id DESC);

CREATE TABLE contents (
    id TEXT COLLATE BINARY PRIMARY KEY NOT NULL,
    audit_id TEXT COLLATE BINARY NOT NULL REFERENCES audits(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK (ordinal >= -1),
    role TEXT CHECK (role IN ('user', 'assistant')),
    phase TEXT,
    truncated INTEGER NOT NULL CHECK (truncated IN (0, 1)),
    blocks TEXT NOT NULL CHECK (json_type(blocks) = 'array'),
    character_count INTEGER NOT NULL CHECK (character_count >= 0),
    block_count INTEGER NOT NULL CHECK (block_count >= 0),
    UNIQUE (audit_id, ordinal)
) STRICT;

CREATE TABLE settings (
    name TEXT PRIMARY KEY NOT NULL,
    value BLOB NOT NULL
) STRICT;
