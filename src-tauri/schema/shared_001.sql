-- persistence/schema/shared_001.sql
-- Instance database schema: shared.db
-- Stores spaces, users, instance config, artifact versions.
-- Not per-user encrypted — must be readable before any user logs in.
-- Contains no personal field values. instance_context limited to
-- general and personal sensitivity only (household name, shared prefs).
-- See ARCHITECTURE Section 8.1 for the explicit design rationale.
-- Migration version: 1

CREATE TABLE IF NOT EXISTS schema_version (
    version         INTEGER PRIMARY KEY,
    applied_at      TEXT NOT NULL,
    description     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS migration_lock (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    locked_at       TEXT,
    locked_by       TEXT
);
INSERT OR IGNORE INTO migration_lock (id) VALUES (1);

-- Spaces
CREATE TABLE IF NOT EXISTS spaces (
    id                      TEXT PRIMARY KEY,
    display_name            TEXT NOT NULL,
    space_type              TEXT NOT NULL,
    privacy_default_tier    INTEGER NOT NULL DEFAULT 1,
    max_permitted_tier      INTEGER NOT NULL DEFAULT 1,
    created_at              TEXT NOT NULL,
    extra_metadata          TEXT NOT NULL DEFAULT '{}',
    CHECK (max_permitted_tier >= privacy_default_tier)
);

-- Users
CREATE TABLE IF NOT EXISTS users (
    id                          TEXT PRIMARY KEY,
    display_name                TEXT NOT NULL UNIQUE,
    role                        TEXT NOT NULL DEFAULT 'builder'
                                    CHECK (role IN ('consumer', 'builder', 'admin')),
    is_primary                  INTEGER NOT NULL DEFAULT 0,
    auth_enabled                INTEGER NOT NULL DEFAULT 0,
    password_hash               TEXT,
    tier2_provider_preference   TEXT
                                    CHECK (tier2_provider_preference IS NULL
                                        OR tier2_provider_preference IN ('mistral', 'groq')),
    created_at                  TEXT NOT NULL,
    extra_metadata              TEXT NOT NULL DEFAULT '{}'
);

-- Enforce only one primary user per instance
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_single_primary
    ON users (is_primary) WHERE is_primary = 1;

-- User salts — includes KDF metadata for future algorithm upgrades
CREATE TABLE IF NOT EXISTS user_salts (
    user_id         TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    salt_hex        TEXT NOT NULL,
    kdf_algorithm   TEXT NOT NULL DEFAULT 'pbkdf2_sha256',
    kdf_iterations  INTEGER NOT NULL DEFAULT 600000,
    created_at      TEXT NOT NULL
);

-- User-space membership
CREATE TABLE IF NOT EXISTS user_spaces (
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    space_id    TEXT NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
    joined_at   TEXT NOT NULL,
    PRIMARY KEY (user_id, space_id)
);

-- Instance-level shared context (general and personal sensitivity ONLY)
CREATE TABLE IF NOT EXISTS instance_context (
    id              TEXT PRIMARY KEY,
    field_name      TEXT NOT NULL,
    field_value     TEXT NOT NULL,
    sensitivity     TEXT NOT NULL CHECK (sensitivity IN ('general', 'personal')),
    created_at      TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

-- Context groups (Release 1 schema, Release 2 UX)
CREATE TABLE IF NOT EXISTS context_groups (
    id              TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    space_id        TEXT REFERENCES spaces(id) ON DELETE CASCADE,
    created_at      TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS context_group_members (
    group_id    TEXT NOT NULL REFERENCES context_groups(id) ON DELETE CASCADE,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    joined_at   TEXT NOT NULL,
    PRIMARY KEY (group_id, user_id)
);

-- Artifact version tracking (spans all spaces)
-- scope: 'space' uses space_id; '_global' for instance-wide artifacts
CREATE TABLE IF NOT EXISTS artifact_versions (
    artifact_type   TEXT NOT NULL,
    artifact_id     TEXT NOT NULL,
    scope           TEXT NOT NULL DEFAULT '_global',
    space_id        TEXT NOT NULL DEFAULT '_global',
    version         TEXT NOT NULL,
    trust_level     TEXT NOT NULL
                        CHECK (trust_level IN ('official', 'reviewed', 'community', 'local_only')),
    revoked         INTEGER NOT NULL DEFAULT 0,
    installed_at    TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (artifact_type, artifact_id, scope, space_id)
);

-- Instance configuration
CREATE TABLE IF NOT EXISTS instance_config (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);

INSERT OR IGNORE INTO instance_config VALUES ('role_enforcement', 'disabled');
INSERT OR IGNORE INTO instance_config VALUES ('auth_lockout_enabled', 'disabled');
INSERT OR IGNORE INTO instance_config VALUES ('instance_name', '');

-- Auth session tables (Release 1: schema present, NOT enforced)
CREATE TABLE IF NOT EXISTS auth_sessions (
    session_id      TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at      TEXT NOT NULL,
    last_active_at  TEXT NOT NULL,
    expires_at      TEXT NOT NULL,
    ip_address      TEXT,
    user_agent      TEXT,
    is_remember_me  INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_auth_sessions_user
    ON auth_sessions (user_id, expires_at);

CREATE TABLE IF NOT EXISTS auth_failures (
    id              TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    attempted_at    TEXT NOT NULL,
    ip_address      TEXT
);

CREATE INDEX IF NOT EXISTS idx_auth_failures_name
    ON auth_failures (display_name, attempted_at DESC);

CREATE TABLE IF NOT EXISTS auth_lockouts (
    display_name    TEXT PRIMARY KEY,
    locked_until    TEXT NOT NULL,
    failure_count   INTEGER NOT NULL DEFAULT 0,
    locked_at       TEXT NOT NULL
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (1, datetime('now'), 'Initial shared.db schema');
