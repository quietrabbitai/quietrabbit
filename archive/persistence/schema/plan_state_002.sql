-- persistence/schema/plan_state_002.sql
-- Phase C: Persona model migration for plan_state.db.
-- Renames life_id -> persona_id in topic_header (D6-298).
-- Part of Persona model migration (D6-289 through D6-303).
--
-- plan_state.db is per-user, per-persona, per-focus, per-topic, encrypted.
-- New path: /users/{user_id}/personas/{persona_id}/focuses/{focus_id}/
--           topics/{topic_id}/plan_state.db
-- Path construction handled in persistence/topic_store.py (task 13) and
-- persistence/migrations.py (tasks 15, 16).
--
-- topic_header is a single-row table (CHECK id = 1) -- cache copy of
-- topic metadata from outputs.db for offline coherence. No indexes beyond
-- the integer primary key. No UNIQUE constraints beyond CHECK (id = 1).
-- plan_state_blocks, handoff_tokens, state_ceiling_status: no life_id
-- column in any of these tables -- not in scope for this migration.
--
-- Strategy: recreate topic_header with renamed column, copy data, drop old.
-- Executed within run_migrations SAVEPOINT.
--
-- Data copy pattern: INSERT INTO (fail loudly on unexpected duplicates).
-- INSERT OR IGNORE used only for schema_version seeding.

-- Step 1: recreate topic_header with persona_id (was life_id)
-- Single-row table: CHECK (id = 1) guard preserved.
-- No named indexes on topic_header -- only integer primary key.
-- All column defaults verified against plan_state_001.sql:
--   lifecycle_state TEXT NOT NULL DEFAULT 'active' -- preserved
--   session_count   INTEGER NOT NULL DEFAULT 0     -- preserved
CREATE TABLE IF NOT EXISTS topic_header_new (
    id                  INTEGER PRIMARY KEY CHECK (id = 1),
    topic_id            TEXT NOT NULL,
    focus_id            TEXT NOT NULL,
    persona_id          TEXT NOT NULL,
    name                TEXT,
    placeholder_name    TEXT NOT NULL,
    lifecycle_state     TEXT NOT NULL DEFAULT 'active',
    current_phase       TEXT,
    session_count       INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);

INSERT INTO topic_header_new
    (id, topic_id, focus_id, persona_id, name, placeholder_name,
     lifecycle_state, current_phase, session_count, created_at, updated_at)
SELECT
    id, topic_id, focus_id, life_id, name, placeholder_name,
    lifecycle_state, current_phase, session_count, created_at, updated_at
FROM topic_header;

DROP TABLE IF EXISTS topic_header;
ALTER TABLE topic_header_new RENAME TO topic_header;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (2, datetime('now'), 'Persona migration: life_id->persona_id in topic_header');
