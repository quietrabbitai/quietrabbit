-- persistence/schema/personal_001.sql
-- Per-user, per-space personal database schema: personal.db
-- Encrypted with SQLCipher using user master key.
-- Path: /users/{user_id}/spaces/{space_id}/personal.db
-- user_id and space_id are encoded in the file path — not repeated
-- in most tables. Kept in disclosure_log only for audit trail integrity.
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

-- Personal fields
-- field_value is BLOB encrypted at field level via HKDF-derived key.
-- sensitivity_severity generated from sensitivity label.
CREATE TABLE IF NOT EXISTS personal_fields (
    id                  TEXT PRIMARY KEY,
    specialist_id       TEXT NOT NULL,
    field_name          TEXT NOT NULL,
    field_value         BLOB NOT NULL,
    sensitivity         TEXT NOT NULL
                            CHECK (sensitivity IN
                                ('general','personal','medical','financial')),
    sensitivity_severity INTEGER NOT NULL GENERATED ALWAYS AS (
                            CASE sensitivity
                                WHEN 'general'   THEN 1
                                WHEN 'personal'  THEN 2
                                WHEN 'medical'   THEN 3
                                WHEN 'financial' THEN 4
                                ELSE 99
                            END
                        ) STORED,
    ownership_scope     TEXT NOT NULL DEFAULT 'self'
                            CHECK (ownership_scope IN
                                ('self','group','instance')),
    abstraction_tier2   TEXT NOT NULL DEFAULT 'pass'
                            CHECK (abstraction_tier2 IN
                                ('pass','omit','summarize',
                                 'range_only','not_permitted')),
    abstraction_tier3   TEXT NOT NULL DEFAULT 'pass'
                            CHECK (abstraction_tier3 IN
                                ('pass','omit','summarize',
                                 'range_only','not_permitted')),
    source              TEXT NOT NULL DEFAULT 'interview',
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_personal_fields_specialist
    ON personal_fields (specialist_id, sensitivity_severity);

-- Personal field groups
-- group_id is a cross-db reference resolved at application layer.
-- Table exists here for local FK enforcement on field_id.
CREATE TABLE IF NOT EXISTS personal_field_groups (
    id          TEXT PRIMARY KEY,
    group_id    TEXT NOT NULL,
    field_id    TEXT NOT NULL REFERENCES personal_fields(id) ON DELETE CASCADE,
    created_at  TEXT NOT NULL
);

-- Voice profiles
-- precedence: 1=model_baseline 2=specialist_defaults 3=global
--             4=space 5=writing_context (highest wins)
CREATE TABLE IF NOT EXISTS voice_profiles (
    id              TEXT PRIMARY KEY,
    space_id        TEXT,       -- NULL = global (all spaces)
    specialist_id   TEXT,       -- NULL = all specialists
    precedence      INTEGER NOT NULL CHECK (precedence BETWEEN 1 AND 5),
    attribute       TEXT NOT NULL,
    value           TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_voice_profiles_lookup
    ON voice_profiles (specialist_id, precedence);

-- Disclosure log — NEVER deleted, permanent audit trail.
-- user_id retained here for audit trail integrity in backup/recovery.
CREATE TABLE IF NOT EXISTS disclosure_log (
    id                  TEXT PRIMARY KEY,
    user_id             TEXT NOT NULL,
    space_id            TEXT NOT NULL,
    path_run_id         TEXT NOT NULL,
    step_id             TEXT NOT NULL,
    routing_tier        INTEGER NOT NULL,
    provider            TEXT,
    fields_shared       TEXT NOT NULL DEFAULT '[]',
    fields_abstracted   TEXT NOT NULL DEFAULT '{}',
    fields_withheld     TEXT NOT NULL DEFAULT '[]',
    override_declined   INTEGER NOT NULL DEFAULT 0,
    declined_at         TEXT,
    created_at          TEXT NOT NULL,
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_disclosure_log_run
    ON disclosure_log (path_run_id, created_at);

-- Staleness check state — single row per database (one per space)
CREATE TABLE IF NOT EXISTS staleness_check_state (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    last_checked_at TEXT NOT NULL,
    fields_stale    TEXT NOT NULL DEFAULT '[]',
    check_result    TEXT NOT NULL DEFAULT 'ok'
                        CHECK (check_result IN ('ok','stale','error')),
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

-- Notifications
CREATE TABLE IF NOT EXISTS notifications (
    id          TEXT PRIMARY KEY,
    severity    TEXT NOT NULL
                    CHECK (severity IN ('info','suggest','require','stop')),
    title       TEXT NOT NULL,
    body        TEXT NOT NULL,
    action_url  TEXT,
    read_at     TEXT,
    created_at  TEXT NOT NULL,
    extra_metadata TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_notifications_unread
    ON notifications (read_at, created_at DESC);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (1, datetime('now'), 'Initial personal.db schema');
