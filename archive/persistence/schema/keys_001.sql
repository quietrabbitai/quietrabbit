-- persistence/schema/keys_001.sql
-- Per-user integration keys database schema: integration_keys.db
-- Encrypted with SQLCipher using user master key.
-- Path: /users/{user_id}/integration_keys.db
-- API keys NEVER stored in environment variables or config files.
-- Migration version: 1

CREATE TABLE IF NOT EXISTS schema_version (
    version         INTEGER PRIMARY KEY,
    applied_at      TEXT NOT NULL,
    description     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS migration_lock (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    locked_at   TEXT,
    locked_by   TEXT
);
INSERT OR IGNORE INTO migration_lock (id) VALUES (1);

-- Integration keys
-- encrypted_key: field-level encrypted API key (BLOB).
-- iv_hex: initialization vector used during encryption.
--   Required for non-deterministic encryption — same key material
--   must not produce the same ciphertext across different keys.
-- key_type: open TEXT — no CHECK constraint.
--   Current values: 'tier2', 'tier3'
--   Phase 2 additions: 'integration' (Notion, Calendar, GitHub, etc.)
--   Application layer validates key_type values.
-- integration_id: distinguishes multiple integrations from same provider
--   (e.g., Google Drive + Gmail = same provider, different integration_id).
CREATE TABLE IF NOT EXISTS integration_keys (
    id              TEXT PRIMARY KEY,
    provider        TEXT NOT NULL,
    key_type        TEXT NOT NULL,
    integration_id  TEXT NOT NULL DEFAULT '_default',
    credential_label TEXT NOT NULL,
    encrypted_key   BLOB NOT NULL,
    iv_hex          TEXT NOT NULL,
    is_active       INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL,
    last_verified_at TEXT,
    extra_metadata  TEXT NOT NULL DEFAULT '{}',
    UNIQUE (provider, key_type, integration_id)
);

CREATE INDEX IF NOT EXISTS idx_integration_keys_lookup
    ON integration_keys (provider, key_type, is_active, last_verified_at);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (1, datetime('now'), 'Initial integration_keys.db schema');
