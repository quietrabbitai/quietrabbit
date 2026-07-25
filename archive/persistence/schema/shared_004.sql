-- persistence/schema/shared_004.sql
-- Phase C: Persona model migration.
-- Renames lives -> personas, removing max_permitted_tier and
-- life_privacy_default_tier from the Persona record (D6-297).
-- Both fields relocated to focus_settings — not eliminated entirely.
-- Renames user_lives -> user_personas.
-- Renames life_id -> persona_id in context_groups, artifact_versions,
-- topic_index, and asset_index.
-- Creates focus_settings table in shared.db (D6-299, D6-302).
-- Seeds focus_settings defaults for existing three Focuses (D6-303).
-- Part of Persona model migration (D6-289 through D6-303).
--
-- Persona model (D6-289, D6-291):
--   Persona is a personalization grouping only -- no tier fields on the record.
--   Tier enforcement is Focus-level, not Persona-level.
--   effective_tier = min(focus_def.max_routing_tier, focus_settings.privacy_tier)
--
-- focus_settings placement in shared.db (D6-299):
--   Privacy Guardian must read Focus settings before opening encrypted
--   per-user stores. Settings are behavioral config, not personal data.
--
-- Strategy: create new tables, copy data, drop old, recreate indexes.
-- Executed within run_migrations SAVEPOINT -- atomic or rolled back.
--
-- Seed note (D6-303): focus_settings seeded for all existing Focuses using
-- the first available persona by created_at. In the dev environment this
-- is 'dev-life' (the existing persona_id carried over from lives). Real users
-- configure focus_settings during onboarding -- seeds are dev-only fixtures.
-- Step 1: create personas table (replaces lives)
-- max_permitted_tier and life_privacy_default_tier removed from Persona record.
-- Both fields relocated to focus_settings (D6-297) -- not eliminated.
-- persona_type retains the same values as life_type -- no value migration needed.
-- extra_metadata carries over from lives unchanged -- includes any stored
-- floor_consent_preference (D5-152). Application layer (lifecycle.py, routes.py)
-- updated in tasks 8 and 18 to read/write personas.extra_metadata.
CREATE TABLE IF NOT EXISTS personas (
    id              TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    persona_type    TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

-- Step 2: copy lives data into personas
-- IDs are preserved unchanged -- path and DB references carry the same value.
-- Storage path segment rename (lives/ -> personas/) is handled in
-- providers/utils.py and persistence/migrations.py (tasks 16, 17).
INSERT OR IGNORE INTO personas
    (id, display_name, persona_type, created_at, extra_metadata)
SELECT
    id, display_name, life_type, created_at, extra_metadata
FROM lives;

-- Step 3: create user_personas table (replaces user_lives)
CREATE TABLE IF NOT EXISTS user_personas (
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    persona_id  TEXT NOT NULL REFERENCES personas(id) ON DELETE CASCADE,
    joined_at   TEXT NOT NULL,
    PRIMARY KEY (user_id, persona_id)
);

-- Step 4: copy user_lives data into user_personas
INSERT OR IGNORE INTO user_personas (user_id, persona_id, joined_at)
SELECT user_id, life_id, joined_at
FROM user_lives;

-- Step 5: recreate context_groups with persona_id (was life_id)
-- No CREATE INDEX statements existed on context_groups in prior migrations
-- (verified against shared_001.sql and shared_002.sql) -- only primary key,
-- which is recreated in the new table definition.
CREATE TABLE IF NOT EXISTS context_groups_new (
    id              TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    persona_id      TEXT REFERENCES personas(id) ON DELETE CASCADE,
    created_at      TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

INSERT OR IGNORE INTO context_groups_new
    (id, display_name, persona_id, created_at, extra_metadata)
SELECT
    id, display_name, life_id, created_at, extra_metadata
FROM context_groups;

DROP TABLE IF EXISTS context_groups;
ALTER TABLE context_groups_new RENAME TO context_groups;

-- Step 6: recreate artifact_versions with persona_id (was life_id)
-- No named indexes existed on artifact_versions in prior migrations
-- (verified against shared_001.sql and shared_002.sql) -- only the composite
-- primary key, which is recreated in the new table definition.
-- Primary key (artifact_type, artifact_id, scope, persona_id) is unchanged
-- in structure; persona_id replaces life_id in the fourth position.
-- _global rows: life_id = '_global' -> persona_id = '_global', preserved.
CREATE TABLE IF NOT EXISTS artifact_versions_new (
    artifact_type   TEXT NOT NULL
                        CHECK (artifact_type IN ('guide', 'operator', 'focus')),
    artifact_id     TEXT NOT NULL,
    scope           TEXT NOT NULL DEFAULT '_global',
    persona_id      TEXT NOT NULL DEFAULT '_global',
    version         TEXT NOT NULL,
    trust_level     TEXT NOT NULL
                        CHECK (trust_level IN
                            ('official', 'reviewed', 'community', 'local_only')),
    revoked         INTEGER NOT NULL DEFAULT 0,
    installed_at    TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (artifact_type, artifact_id, scope, persona_id)
);

INSERT OR IGNORE INTO artifact_versions_new
    (artifact_type, artifact_id, scope, persona_id, version,
     trust_level, revoked, installed_at, extra_metadata)
SELECT
    artifact_type, artifact_id, scope, life_id, version,
    trust_level, revoked, installed_at, extra_metadata
FROM artifact_versions;

DROP TABLE IF EXISTS artifact_versions;
ALTER TABLE artifact_versions_new RENAME TO artifact_versions;

-- Step 7: recreate topic_index with persona_id (was life_id)
-- topic_index added in shared_003.sql. life_id was a plain TEXT column --
-- no FK constraint in original. New table retains same non-FK design.
-- Index renamed: idx_topic_index_life -> idx_topic_index_persona.
-- idx_topic_index_focus name unchanged, recreated on new table.
CREATE TABLE IF NOT EXISTS topic_index_new (
    topic_id            TEXT PRIMARY KEY,
    persona_id          TEXT NOT NULL,
    focus_id            TEXT NOT NULL,
    display_name        TEXT NOT NULL,
    lifecycle_state     TEXT NOT NULL
                            CHECK (lifecycle_state IN (
                                'active', 'paused', 'awaiting',
                                'complete', 'closed'
                            )),
    last_active_at      TEXT NOT NULL,
    session_count       INTEGER NOT NULL DEFAULT 0,
    content_summary     TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);

INSERT OR IGNORE INTO topic_index_new
    (topic_id, persona_id, focus_id, display_name, lifecycle_state,
     last_active_at, session_count, content_summary, created_at, updated_at)
SELECT
    topic_id, life_id, focus_id, display_name, lifecycle_state,
    last_active_at, session_count, content_summary, created_at, updated_at
FROM topic_index;

DROP TABLE IF EXISTS topic_index;
ALTER TABLE topic_index_new RENAME TO topic_index;

CREATE INDEX IF NOT EXISTS idx_topic_index_persona
    ON topic_index (persona_id, lifecycle_state, last_active_at DESC);

CREATE INDEX IF NOT EXISTS idx_topic_index_focus
    ON topic_index (focus_id, lifecycle_state);

-- Step 8: recreate asset_index with persona_id (was life_id)
-- asset_index added in shared_003.sql. life_id was a plain TEXT column --
-- no FK constraint in original. New table retains same non-FK design.
-- Index renamed: idx_asset_index_life -> idx_asset_index_persona.
CREATE TABLE IF NOT EXISTS asset_index_new (
    asset_id            TEXT PRIMARY KEY,
    persona_id          TEXT NOT NULL,
    focus_id            TEXT,
    asset_type          TEXT NOT NULL
                            CHECK (asset_type IN ('static', 'structured')),
    backing_type        TEXT NOT NULL DEFAULT 'local'
                            CHECK (backing_type IN ('local', 'imported', 'connected')),
    name                TEXT NOT NULL,
    name_sensitivity    TEXT NOT NULL DEFAULT 'standard'
                            CHECK (name_sensitivity IN (
                                'standard', 'sensitive', 'private', 'locked'
                            )),
    content_ref         TEXT,
    created_at          TEXT NOT NULL,
    last_modified_at    TEXT NOT NULL,
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
);

INSERT OR IGNORE INTO asset_index_new
    (asset_id, persona_id, focus_id, asset_type, backing_type, name,
     name_sensitivity, content_ref, created_at, last_modified_at, extra_metadata)
SELECT
    asset_id, life_id, focus_id, asset_type, backing_type, name,
    name_sensitivity, content_ref, created_at, last_modified_at, extra_metadata
FROM asset_index;

DROP TABLE IF EXISTS asset_index;
ALTER TABLE asset_index_new RENAME TO asset_index;

CREATE INDEX IF NOT EXISTS idx_asset_index_persona
    ON asset_index (persona_id, focus_id, last_modified_at DESC);

-- Step 9: drop old tables
-- user_lives dropped first (FK to lives -- drop child before parent).
-- context_groups was already recreated above referencing personas, not lives.
DROP TABLE IF EXISTS user_lives;
DROP TABLE IF EXISTS lives;

-- Step 10: create focus_settings table (NEW -- D6-291, D6-299, D6-302)
-- Stores three independent Focus settings per D6-291:
--   context_flow:       bidirectional (default) | receive_only | isolated
--   library_visibility: shared (default) | persona_visible | persona_hidden
--   privacy_tier:       1=red | 2=yellow (default) | 3=green
-- max_permitted_tier: hard Tier ceiling for this Focus -- relocated from Persona
--   per D6-297. Enforced at AUTHORIZE in conductor/lifecycle.py.
-- focus_profile: convenience label mapping the three settings (D6-294):
--   open       = bidirectional + shared          + yellow (default)
--   organized  = bidirectional + persona_visible + yellow
--   protected  = receive_only  + persona_hidden  + red
-- voice_override: Focus-level voice JSON, overrides Persona baseline (D6-302).
--   NULL = inherit Persona voice profile. Topic-level overrides deferred Phase D.
--
-- Conductor asserts this row exists at AUTHORIZE -- fails with clear error if
-- absent, not null-pointer downstream (D6-303).
-- Real users configure focus_settings during onboarding. Step 11 seeds dev rows.
CREATE TABLE IF NOT EXISTS focus_settings (
    persona_id          TEXT NOT NULL REFERENCES personas(id) ON DELETE CASCADE,
    focus_id            TEXT NOT NULL,
    context_flow        TEXT NOT NULL DEFAULT 'bidirectional'
                            CHECK (context_flow IN (
                                'bidirectional', 'receive_only', 'isolated'
                            )),
    library_visibility  TEXT NOT NULL DEFAULT 'shared'
                            CHECK (library_visibility IN (
                                'shared', 'persona_visible', 'persona_hidden'
                            )),
    privacy_tier        INTEGER NOT NULL DEFAULT 2
                            CHECK (privacy_tier BETWEEN 1 AND 3),
    max_permitted_tier  INTEGER NOT NULL DEFAULT 2
                            CHECK (max_permitted_tier BETWEEN 1 AND 3),
    focus_profile       TEXT NOT NULL DEFAULT 'open'
                            CHECK (focus_profile IN (
                                'open', 'organized', 'protected'
                            )),
    voice_override      TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    PRIMARY KEY (persona_id, focus_id)
);

-- Primary read path: get_focus_settings(focus_id) in focus_settings_store.py.
CREATE INDEX IF NOT EXISTS idx_focus_settings_focus_id
    ON focus_settings (focus_id);

-- Step 11: seed focus_settings defaults for existing three Focuses (D6-303)
-- All three seeded as Open profile: bidirectional, shared, yellow, max_permitted_tier=2.
-- In dev (single persona): all three rows seed to the same persona_id.
-- In production: focus_settings are created during onboarding.
-- One INSERT with CROSS JOIN -- single seed operation, three rows.
-- SELECT from personas ORDER BY created_at LIMIT 1: deterministic pick of
-- the first persona. INSERT OR IGNORE: safe on re-runs and empty personas table.
INSERT OR IGNORE INTO focus_settings
    (persona_id, focus_id, context_flow, library_visibility,
     privacy_tier, max_permitted_tier, focus_profile, voice_override,
     created_at, updated_at)
SELECT
    p.id, f.focus_id, 'bidirectional', 'shared', 2, 2, 'open', NULL,
    datetime('now'), datetime('now')
FROM (SELECT id FROM personas ORDER BY created_at LIMIT 1) p
CROSS JOIN (
    SELECT 'research-and-buy' AS focus_id UNION ALL
    SELECT 'quick-ask'                     UNION ALL
    SELECT 'writing-assistant'
) f;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (4, datetime('now'), 'Persona model migration: lives->personas, user_lives->user_personas, life_id->persona_id in shared.db, focus_settings table added');
