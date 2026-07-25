-- persistence/schema/personal_003.sql
-- Migration 3: rename legacy terminology in personal.db.
--
-- personal_fields:  specialist_id → source_id
-- voice_profiles:   space_id → life_id, specialist_id → source_id
-- disclosure_log:   space_id → life_id, path_run_id → focus_run_id
-- staleness_check_state: no column renames needed
-- notifications: no column renames needed
--
-- Part of Phase A codebase rename (D6-224, D6-225).
--
-- Strategy: recreate tables with renamed columns, copy data, drop old tables.
-- Executed within run_migrations SAVEPOINT — atomic or rolled back.
--
-- Note on personal_fields indexes:
--   idx_personal_fields_specialist references specialist_id column.
--   Dropped implicitly when the table is dropped and recreated with source_id.
--
-- Note on disclosure_log:
--   Permanent audit trail — never deleted. All historical records are
--   migrated with their original values preserved. path_run_id values
--   (UUIDs) are unchanged — only the column name changes.

-- Step 1: recreate personal_fields with source_id
CREATE TABLE IF NOT EXISTS personal_fields_new (
    id                  TEXT PRIMARY KEY,
    source_id           TEXT NOT NULL,
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

INSERT INTO personal_fields_new
    (id, source_id, field_name, field_value, sensitivity,
     ownership_scope, abstraction_tier2, abstraction_tier3,
     source, created_at, updated_at, extra_metadata)
SELECT
    id, specialist_id, field_name, field_value, sensitivity,
    ownership_scope, abstraction_tier2, abstraction_tier3,
    source, created_at, updated_at, extra_metadata
FROM personal_fields;

DROP TABLE IF EXISTS personal_fields;

ALTER TABLE personal_fields_new RENAME TO personal_fields;

CREATE INDEX IF NOT EXISTS idx_personal_fields_source
    ON personal_fields (source_id, sensitivity_severity);

-- Step 2: recreate voice_profiles with life_id and source_id
CREATE TABLE IF NOT EXISTS voice_profiles_new (
    id              TEXT PRIMARY KEY,
    life_id         TEXT,       -- NULL = global (all lives)
    source_id       TEXT,       -- NULL = all sources
    precedence      INTEGER NOT NULL CHECK (precedence BETWEEN 1 AND 5),
    attribute       TEXT NOT NULL,
    value           TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

INSERT INTO voice_profiles_new
    (id, life_id, source_id, precedence,
     attribute, value, created_at, updated_at, extra_metadata)
SELECT
    id, space_id, specialist_id, precedence,
    attribute, value, created_at, updated_at, extra_metadata
FROM voice_profiles;

DROP TABLE IF EXISTS voice_profiles;

ALTER TABLE voice_profiles_new RENAME TO voice_profiles;

CREATE INDEX IF NOT EXISTS idx_voice_profiles_lookup
    ON voice_profiles (source_id, precedence);

-- Step 3: recreate disclosure_log with life_id and focus_run_id
-- Permanent audit table — all historical records migrated intact.
CREATE TABLE IF NOT EXISTS disclosure_log_new (
    id                  TEXT PRIMARY KEY,
    user_id             TEXT NOT NULL,
    life_id             TEXT NOT NULL,
    focus_run_id        TEXT NOT NULL,
    step_id             TEXT NOT NULL,
    routing_tier        INTEGER NOT NULL,
    provider            TEXT,
    fields_shared       TEXT NOT NULL DEFAULT '[]',
    fields_abstracted   TEXT NOT NULL DEFAULT '{}',
    fields_withheld     TEXT NOT NULL DEFAULT '[]',
    override_declined   INTEGER NOT NULL DEFAULT 0,
    declined_at         TEXT,
    created_at          TEXT NOT NULL,
    extra_metadata      TEXT NOT NULL DEFAULT '{}',
    execution_tier      INTEGER,
    abstraction_tier    INTEGER
);

INSERT INTO disclosure_log_new
    (id, user_id, life_id, focus_run_id, step_id, routing_tier,
     provider, fields_shared, fields_abstracted, fields_withheld,
     override_declined, declined_at, created_at, extra_metadata,
     execution_tier, abstraction_tier)
SELECT
    id, user_id, space_id, path_run_id, step_id, routing_tier,
    provider, fields_shared, fields_abstracted, fields_withheld,
    override_declined, declined_at, created_at, extra_metadata,
    execution_tier, abstraction_tier
FROM disclosure_log;

DROP TABLE IF EXISTS disclosure_log;

ALTER TABLE disclosure_log_new RENAME TO disclosure_log;

CREATE INDEX IF NOT EXISTS idx_disclosure_log_run
    ON disclosure_log (focus_run_id, created_at);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (3, datetime('now'),
    'Rename specialist_id to source_id, space_id to life_id, path_run_id to focus_run_id in personal.db');
