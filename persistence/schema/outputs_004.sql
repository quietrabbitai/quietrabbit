-- persistence/schema/outputs_004.sql
-- Phase C: Persona model migration for outputs.db.
-- Renames life_id -> persona_id in topics, run_history,
-- and classification_preferences (D6-298).
-- Updates UNIQUE constraint on classification_preferences.
-- Updates topic_storage_locations.db_path path segment:
--   /lives/{life_id}/ -> /personas/{persona_id}/
-- Part of Persona model migration (D6-289 through D6-303).
--
-- outputs.db is per-user, per-persona, encrypted with SQLCipher.
-- New path: /users/{user_id}/personas/{persona_id}/outputs.db
-- Path construction handled in providers/utils.py and
-- persistence/migrations.py (tasks 16, 17).
--
-- Strategy: recreate tables with renamed column, copy data, drop old,
-- recreate indexes. Executed within run_migrations SAVEPOINT.
--
-- FK notes:
--   topic_storage_locations.topic_id REFERENCES topics(id) ON DELETE CASCADE
--   run_history.topic_id             REFERENCES topics(id)
--   focus_runs.topic_id              REFERENCES topics(id)
--   All FK references heal by table name after drop/rename cycle.
--   migrations.py has no PRAGMA foreign_keys = ON -- no runtime risk.
--   Application layer (topic_store.py, lifecycle.py) updated in tasks 13, 8.
--
-- Data copy pattern: INSERT INTO (not INSERT OR IGNORE) -- fails loudly on
-- unexpected duplicates. Matches shared_002.sql and outputs_002.sql precedent.
-- INSERT OR IGNORE used only for schema_version seeding.

-- Step 1: recreate topics with persona_id (was life_id)
-- topics added in outputs_003.sql.
-- Index renamed: idx_topics_life -> idx_topics_persona.
-- idx_topics_focus name unchanged.
-- topics(id) FK references in topic_storage_locations, run_history, and
-- focus_runs heal by table name after rename. Data integrity by value.
CREATE TABLE IF NOT EXISTS topics_new (
    id                  TEXT PRIMARY KEY,
    focus_id            TEXT NOT NULL,
    user_id             TEXT NOT NULL,
    persona_id          TEXT NOT NULL,
    name                TEXT,
    placeholder_name    TEXT NOT NULL,
    lifecycle_state     TEXT NOT NULL DEFAULT 'active'
                            CHECK (lifecycle_state IN (
                                'active', 'paused', 'awaiting',
                                'complete', 'closed'
                            )),
    dormant_since       TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    closed_at           TEXT,
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
);

INSERT INTO topics_new
    (id, focus_id, user_id, persona_id, name, placeholder_name,
     lifecycle_state, dormant_since, created_at, updated_at,
     closed_at, extra_metadata)
SELECT
    id, focus_id, user_id, life_id, name, placeholder_name,
    lifecycle_state, dormant_since, created_at, updated_at,
    closed_at, extra_metadata
FROM topics;

DROP TABLE IF EXISTS topics;
ALTER TABLE topics_new RENAME TO topics;

CREATE INDEX IF NOT EXISTS idx_topics_focus
    ON topics (focus_id, lifecycle_state, updated_at DESC);

CREATE INDEX IF NOT EXISTS idx_topics_persona
    ON topics (persona_id, lifecycle_state, updated_at DESC);

-- Step 2: recreate run_history with persona_id (was life_id)
-- run_history added in outputs_003.sql.
-- Index names do not reference life_id -- recreated unchanged.
CREATE TABLE IF NOT EXISTS run_history_new (
    id                          TEXT PRIMARY KEY,
    focus_run_id                TEXT NOT NULL REFERENCES focus_runs(id),
    focus_id                    TEXT NOT NULL,
    persona_id                  TEXT NOT NULL,
    topic_id                    TEXT REFERENCES topics(id),
    output_id                   TEXT,
    output_type                 TEXT,
    is_quick_ask                INTEGER NOT NULL DEFAULT 0,
    promote_window_expires_at   TEXT,
    created_at                  TEXT NOT NULL,
    extra_metadata              TEXT NOT NULL DEFAULT '{}'
);

INSERT INTO run_history_new
    (id, focus_run_id, focus_id, persona_id, topic_id, output_id,
     output_type, is_quick_ask, promote_window_expires_at,
     created_at, extra_metadata)
SELECT
    id, focus_run_id, focus_id, life_id, topic_id, output_id,
    output_type, is_quick_ask, promote_window_expires_at,
    created_at, extra_metadata
FROM run_history;

DROP TABLE IF EXISTS run_history;
ALTER TABLE run_history_new RENAME TO run_history;

CREATE INDEX IF NOT EXISTS idx_run_history_focus
    ON run_history (focus_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_run_history_topic
    ON run_history (topic_id, created_at DESC)
    WHERE topic_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_run_history_promote
    ON run_history (promote_window_expires_at)
    WHERE promote_window_expires_at IS NOT NULL
    AND topic_id IS NULL
    AND is_quick_ask = 0;

-- Step 3: recreate classification_preferences with persona_id (was life_id)
-- classification_preferences added in outputs_003.sql.
-- UNIQUE constraint updated: (focus_id, life_id, content_type)
--   -> (focus_id, persona_id, content_type)
-- idx_classification_prefs_lookup column reference updated accordingly.
-- Persona IDs are preserved from life IDs (D6-298) -- no behavioral
-- change for existing data on the uniqueness constraint.
CREATE TABLE IF NOT EXISTS classification_preferences_new (
    id                  TEXT PRIMARY KEY,
    focus_id            TEXT NOT NULL,
    persona_id          TEXT NOT NULL,
    content_type        TEXT NOT NULL,
    visibility_scope    TEXT NOT NULL
                            CHECK (visibility_scope IN (
                                'tier_1_only', 'anonymous_tier2',
                                'tier2_permitted', 'tier3_permitted'
                            )),
    transformation      TEXT NOT NULL
                            CHECK (transformation IN (
                                'no_generalize', 'generalize_ok',
                                'anonymize_ok', 'no_transform'
                            )),
    sensitivity_preset  TEXT
                            CHECK (sensitivity_preset IS NULL OR
                                sensitivity_preset IN (
                                'standard', 'sensitive', 'private', 'locked'
                            )),
    user_calibrated     INTEGER NOT NULL DEFAULT 0,
    confidence          REAL NOT NULL DEFAULT 1.0,
    last_applied_at     TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    extra_metadata      TEXT NOT NULL DEFAULT '{}',
    UNIQUE (focus_id, persona_id, content_type)
);

INSERT INTO classification_preferences_new
    (id, focus_id, persona_id, content_type, visibility_scope, transformation,
     sensitivity_preset, user_calibrated, confidence, last_applied_at,
     created_at, updated_at, extra_metadata)
SELECT
    id, focus_id, life_id, content_type, visibility_scope, transformation,
    sensitivity_preset, user_calibrated, confidence, last_applied_at,
    created_at, updated_at, extra_metadata
FROM classification_preferences;

DROP TABLE IF EXISTS classification_preferences;
ALTER TABLE classification_preferences_new RENAME TO classification_preferences;

CREATE INDEX IF NOT EXISTS idx_classification_prefs_lookup
    ON classification_preferences (focus_id, persona_id, content_type);

-- Step 4: update topic_storage_locations.db_path path segment.
-- db_path format was: /users/{user_id}/lives/{life_id}/focuses/...
-- db_path format now:  /users/{user_id}/personas/{persona_id}/focuses/...
-- Path strings generated exclusively by ensure_focus_dirs() and
-- get_domain_context_path() -- no other /lives/ segment source exists.
-- REPLACE is safe and a no-op on empty table (dev wipe-and-rebuild).
UPDATE topic_storage_locations
SET db_path = REPLACE(db_path, '/lives/', '/personas/');

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (4, datetime('now'), 'Persona migration: life_id->persona_id in topics, run_history, classification_preferences; db_path updated in topic_storage_locations');
