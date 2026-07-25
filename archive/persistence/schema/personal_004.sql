-- persistence/schema/personal_004.sql
-- Phase C: Persona model migration for personal.db.
-- Renames life_id -> persona_id in voice_profiles and disclosure_log (D6-298).
-- Part of Persona model migration (D6-289 through D6-303).
--
-- personal.db is per-user, per-persona, encrypted with SQLCipher.
-- New path: /users/{user_id}/personas/{persona_id}/personal.db
-- Path construction handled in providers/utils.py and
-- persistence/migrations.py (tasks 16, 17).
--
-- Strategy: recreate tables with renamed column, copy data, drop old,
-- recreate indexes. Executed within run_migrations SAVEPOINT.
--
-- voice_profiles NULL semantics (D6-293):
--   persona_id NULL = global voice profile (applies to all Personas).
--   This was life_id NULL = global in prior schema. Semantic preserved.
--   personal_store._resolve_voice_profile queries WHERE persona_id = ?
--   OR persona_id IS NULL -- updated in task 11.
--
-- disclosure_log is a permanent audit trail -- all historical records
-- migrated with their original values intact. persona_id = life_id UUID
-- (D6-298: IDs preserved unchanged).
--
-- personal_fields: not in scope -- uses source_id column, not life_id.
-- Correctly handled in personal_003.sql. No changes needed.
--
-- Data copy pattern: INSERT INTO (fail loudly on unexpected duplicates).
-- INSERT OR IGNORE used only for schema_version seeding.
-- Constraint verification against personal_003.sql:
--   voice_profiles: CHECK (precedence BETWEEN 1 AND 5) preserved.
--     No UNIQUE constraints in original -- none added.
--   disclosure_log: no UNIQUE or CHECK constraints in original beyond PK.

-- Step 1: recreate voice_profiles with persona_id (was life_id)
-- voice_profiles added in personal_001.sql, columns renamed in personal_003.sql.
-- persona_id NULL preserved: NULL = global profile, applies to all Personas.
-- NOT NULL intentionally omitted -- matches original and preserves fallback
-- query: WHERE persona_id = ? OR persona_id IS NULL (personal_store.py task 11).
-- idx_voice_profiles_lookup indexes (source_id, precedence) -- no life_id
-- reference in index definition. Name and columns unchanged.
CREATE TABLE IF NOT EXISTS voice_profiles_new (
    id              TEXT PRIMARY KEY,
    persona_id      TEXT,       -- NULL = global (all personas)
    source_id       TEXT,       -- NULL = all sources
    precedence      INTEGER NOT NULL CHECK (precedence BETWEEN 1 AND 5),
    attribute       TEXT NOT NULL,
    value           TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

INSERT INTO voice_profiles_new
    (id, persona_id, source_id, precedence, attribute, value,
     created_at, updated_at, extra_metadata)
SELECT
    id, life_id, source_id, precedence, attribute, value,
    created_at, updated_at, extra_metadata
FROM voice_profiles;

DROP TABLE IF EXISTS voice_profiles;
ALTER TABLE voice_profiles_new RENAME TO voice_profiles;

CREATE INDEX IF NOT EXISTS idx_voice_profiles_lookup
    ON voice_profiles (source_id, precedence);

-- Step 2: recreate disclosure_log with persona_id (was life_id)
-- disclosure_log added in personal_001.sql, columns renamed in personal_003.sql.
-- Permanent audit trail -- all historical records migrated intact.
-- persona_id NOT NULL preserved: every disclosure event belongs to a Persona.
-- idx_disclosure_log_run indexes (focus_run_id, created_at) -- no life_id
-- reference in index definition. Name and columns unchanged.
CREATE TABLE IF NOT EXISTS disclosure_log_new (
    id                  TEXT PRIMARY KEY,
    user_id             TEXT NOT NULL,
    persona_id          TEXT NOT NULL,
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
    (id, user_id, persona_id, focus_run_id, step_id, routing_tier,
     provider, fields_shared, fields_abstracted, fields_withheld,
     override_declined, declined_at, created_at, extra_metadata,
     execution_tier, abstraction_tier)
SELECT
    id, user_id, life_id, focus_run_id, step_id, routing_tier,
    provider, fields_shared, fields_abstracted, fields_withheld,
    override_declined, declined_at, created_at, extra_metadata,
    execution_tier, abstraction_tier
FROM disclosure_log;

DROP TABLE IF EXISTS disclosure_log;
ALTER TABLE disclosure_log_new RENAME TO disclosure_log;

CREATE INDEX IF NOT EXISTS idx_disclosure_log_run
    ON disclosure_log (focus_run_id, created_at);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (4, datetime('now'), 'Persona migration: life_id->persona_id in voice_profiles and disclosure_log');
