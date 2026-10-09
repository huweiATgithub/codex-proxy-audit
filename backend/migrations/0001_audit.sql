CREATE TABLE prompt_audit.audits (
    id TEXT COLLATE "C" PRIMARY KEY,
    identity_scope TEXT NOT NULL,
    identity_key TEXT,
    identity_basis TEXT NOT NULL,
    observation_status TEXT NOT NULL,
    source_position BIGINT NOT NULL CHECK (source_position >= 0),
    policy_version INTEGER NOT NULL CHECK (policy_version > 0),
    key_id TEXT COLLATE "C" NOT NULL,
    key_name TEXT,
    key_name_observed_at TIMESTAMPTZ,
    first_observed_at TIMESTAMPTZ NOT NULL,
    request_id TEXT,
    thread_id TEXT,
    session_id TEXT,
    turn_id TEXT,
    input_kinds JSON NOT NULL,
    repositories JSON,
    input_content_id TEXT COLLATE "C" NOT NULL,
    input_text TEXT NOT NULL,
    input_search TEXT NOT NULL,
    preview TEXT NOT NULL,
    context_count BIGINT NOT NULL CHECK (context_count >= 0),
    context_gap BOOLEAN NOT NULL,
    CHECK (json_typeof(input_kinds) = 'array'),
    CHECK (repositories IS NULL OR json_typeof(repositories) = 'array')
);

-- Only identities established by the collector receive a uniqueness constraint.
-- Uncertain observations carry NULL and must not be collapsed by a text hash.
CREATE UNIQUE INDEX audit_identity ON prompt_audit.audits (identity_scope, identity_key)
    WHERE identity_key IS NOT NULL;
CREATE INDEX audit_time ON prompt_audit.audits (first_observed_at DESC, id DESC);
CREATE INDEX audit_key_time ON prompt_audit.audits (key_id, first_observed_at DESC, id DESC);

-- Metadata is fetched without blocks by detail/list queries. The immutable body
-- never depends on the rolling session state below.
CREATE TABLE prompt_audit.contents (
    id TEXT COLLATE "C" PRIMARY KEY,
    audit_id TEXT COLLATE "C" NOT NULL REFERENCES prompt_audit.audits(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK (ordinal >= -1),
    role TEXT CHECK (role IN ('user', 'assistant')),
    phase TEXT,
    truncated BOOLEAN NOT NULL,
    blocks JSON NOT NULL CHECK (json_typeof(blocks) = 'array'),
    character_count BIGINT NOT NULL CHECK (character_count >= 0),
    block_count BIGINT NOT NULL CHECK (block_count >= 0),
    UNIQUE (audit_id, ordinal)
);

CREATE TABLE prompt_audit.sessions (
    scope TEXT PRIMARY KEY,
    state JSON NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE prompt_audit.key_directory (
    id TEXT COLLATE "C" PRIMARY KEY,
    present BOOLEAN NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE prompt_audit.key_names (
    key_id TEXT COLLATE "C" NOT NULL REFERENCES prompt_audit.key_directory(id),
    name TEXT COLLATE "C" NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (key_id, name)
);

CREATE TABLE prompt_audit.settings (
    name TEXT PRIMARY KEY,
    value BYTEA NOT NULL
);
