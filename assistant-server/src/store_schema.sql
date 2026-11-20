-- One row per business entity. Generated columns keep searchable fields in sync
-- with the Rust record format used by legacy imports and existing callers.
CREATE TABLE users (
    id TEXT PRIMARY KEY NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    email TEXT GENERATED ALWAYS AS (json_extract(payload,'$.email')) STORED NOT NULL UNIQUE,
    username TEXT GENERATED ALWAYS AS (json_extract(payload,'$.username')) STORED NOT NULL,
    active INTEGER GENERATED ALWAYS AS (json_extract(payload,'$.is_active')) STORED NOT NULL
);

CREATE TABLE user_configs (
    id TEXT PRIMARY KEY NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE user_provider_configs (
    user_id TEXT NOT NULL REFERENCES user_configs(id) ON DELETE CASCADE,
    provider TEXT NOT NULL CHECK(provider IN ('openai','anthropic','aliyun','deepseek','mimo')),
    model TEXT,
    key_id INTEGER,
    secret BLOB,
    CHECK ((key_id IS NULL) = (secret IS NULL)),
    PRIMARY KEY (user_id, provider)
);

CREATE TABLE conversations (
    id TEXT PRIMARY KEY NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    owner TEXT GENERATED ALWAYS AS (json_extract(payload,'$.user_id')) STORED NOT NULL,
    updated_at TEXT GENERATED ALWAYS AS (json_extract(payload,'$.updated_at')) STORED NOT NULL
);

CREATE TABLE messages (
    id TEXT PRIMARY KEY NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    owner TEXT GENERATED ALWAYS AS (json_extract(payload,'$.conversation_id')) STORED NOT NULL,
    created_at TEXT GENERATED ALWAYS AS (json_extract(payload,'$.created_at')) STORED NOT NULL
);

CREATE TABLE wechat_bindings (
    id TEXT PRIMARY KEY NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    owner TEXT GENERATED ALWAYS AS (json_extract(payload,'$.user_id')) STORED NOT NULL,
    openid TEXT GENERATED ALWAYS AS (json_extract(payload,'$.openid')) STORED NOT NULL,
    updated_at TEXT GENERATED ALWAYS AS (json_extract(payload,'$.updated_at')) STORED NOT NULL,
    UNIQUE(owner,openid)
);

CREATE TABLE wechat_channels (
    id TEXT PRIMARY KEY NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE oauth_states (
    id TEXT PRIMARY KEY NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE pkuvideo_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    owner TEXT GENERATED ALWAYS AS (json_extract(payload,'$.user_id')) STORED NOT NULL,
    status TEXT GENERATED ALWAYS AS (json_extract(payload,'$.status')) STORED NOT NULL,
    standalone INTEGER GENERATED ALWAYS AS (json_extract(payload,'$.course_id') IS NULL AND json_extract(payload,'$.artifacts_deleted_at') IS NULL) STORED NOT NULL,
    created_at TEXT GENERATED ALWAYS AS (json_extract(payload,'$.created_at')) STORED NOT NULL
);

CREATE TABLE pkuvideo_fw_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    owner TEXT GENERATED ALWAYS AS (json_extract(payload,'$.user_id')) STORED NOT NULL,
    created_at TEXT GENERATED ALWAYS AS (json_extract(payload,'$.created_at')) STORED NOT NULL
);

CREATE INDEX users_by_username ON users(username);
CREATE INDEX conversations_by_owner ON conversations(owner, updated_at DESC, id DESC);
CREATE INDEX messages_by_conversation ON messages(owner, created_at, id);
CREATE INDEX wechat_bindings_by_owner ON wechat_bindings(owner, updated_at DESC, id DESC);
CREATE INDEX pkuvideo_jobs_by_owner ON pkuvideo_jobs(owner, created_at DESC, id DESC);
CREATE INDEX pkuvideo_jobs_standalone ON pkuvideo_jobs(owner) WHERE standalone = 1;
CREATE INDEX pkuvideo_fw_jobs_by_owner ON pkuvideo_fw_jobs(owner, created_at DESC, id DESC);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL,
    user_agent TEXT,
    ip_address TEXT
);
CREATE INDEX sessions_by_user ON sessions(user_id, expires_at);
CREATE INDEX sessions_by_expiry ON sessions(expires_at);

CREATE TABLE oidc_identities (
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    user_id TEXT NOT NULL,
    PRIMARY KEY (issuer, subject),
    UNIQUE (issuer, user_id)
);

CREATE TABLE registration_whitelist (email TEXT PRIMARY KEY NOT NULL);

CREATE TABLE translation_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    owner TEXT GENERATED ALWAYS AS (json_extract(payload,'$.user_id')) STORED NOT NULL,
    status TEXT GENERATED ALWAYS AS (json_extract(payload,'$.status')) STORED NOT NULL,
    updated_at TEXT GENERATED ALWAYS AS (json_extract(payload,'$.updated_at')) STORED NOT NULL
);
CREATE INDEX translation_jobs_by_owner ON translation_jobs(owner, updated_at DESC, id DESC);

CREATE TABLE store_documents (
    name TEXT PRIMARY KEY NOT NULL,
    payload TEXT CHECK (payload IS NULL OR json_valid(payload))
);
