-- view_cache_001.sql
--
-- items.id=304: VIEW-ONLY persona sharing's read-only cache (decisions.id=723).
-- New schema family -- no existing table to mirror. One view_cache.db per
-- (recipient_user_id, share_id), at
-- users/{user_id}/persona_view_shares/{share_id}/view_cache.db, encrypted
-- with the recipient account's own master-key hex -- the same key that
-- already encrypts every personal.db under that account (personal_store::
-- open_personal_db keys every Persona's personal.db with the raw account
-- master key, not a per-persona derived key; only the *path* varies by
-- persona_id). migrate_keys_db / migrate_tier3_cookies_db already establish
-- the precedent of a per-account (not per-persona) encrypted file keyed the
-- same way; migrate_group_db already establishes adding an extra path
-- segment for a second identifier beyond user_id. This combines both: no new
-- key material, no new KeyRegistry plumbing, just a new path shape.
--
-- Deliberately NOT a Persona's personal.db, and NOT a shared.db table: this
-- content is real personal identity+fact data (potentially sensitive
-- depending on Persona type), so it cannot live in shared.db (unencrypted,
-- readable before login); it also is not a Persona (decisions.id=723's whole
-- point), so it cannot live under personas/{persona_id}/.
--
-- SHAPE: mirrors the exact wire structs the payload already carries
-- (Entity's displayable fields, SharedEntityFact, SharedVoiceProfileEntry --
-- auth::persona_sharing.rs / persona_sync::engine.rs), not the real
-- entities/entity_facts/voice_profiles tables those structs are normally
-- read from. Columns that only make sense for a locally-editable, source-
-- reconciled record are dropped entirely: no modification_state, no
-- source_registry_id (nothing to protect from local edits -- there is no
-- local edit path at all), no cross_persona_export/source_persona_id/
-- origin_persona_id (provenance columns; this cache never feeds another
-- Persona's fact ingestion, it only ever gets fully replaced from the one
-- owner it's scoped to).
--
-- REPLACE SEMANTICS: every pull deletes all rows in the three content tables
-- and re-inserts the new snapshot whole (decisions.id=723: "no merge,
-- nothing recipient-editable"), inside one SAVEPOINT -- unlike SYNCED
-- materialization/apply (which must split across personal.db + shared.db
-- SAVEPOINTs, no single SAVEPOINT spans two files), this is one file, so
-- that SAVEPOINT gives real atomicity, no idempotency-not-atomicity caveat
-- needed here.
--
-- view_cache_meta: singleton row (id fixed at 1, CHECK-enforced) carrying
-- the share's own identity and lifecycle state, since that state belongs
-- with the cached content it describes, not with the unencrypted shared.db
-- bookkeeping row (persona_view_share_settings, shared_010.sql) which only
-- tracks *this install's* folder-sync mechanics (where to look, last attempt
-- outcome) -- a different concern.
--   status: 'active' | 'ended'. 'ended' is a positive, permanent state
--     (decisions.id=723: "not silent staleness") set by applying a Revoked
--     tombstone payload -- see persona_view_sync::engine's own header.
--   last_synced_at: the applied payload's own emitted_at (not "now") --
--     same reasoning persona_sync's own last_synced_at column already
--     documents: filesystem mtimes are not trustworthy across heterogeneous
--     NAS/cloud-sync clients.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.
--
-- schema_version: NOT bootstrapped generically by the migration runner
-- (unlike migration_lock, which run_migrations::bootstrap_lock_table creates
-- for every db file regardless of prefix) -- every _001.sql file across this
-- codebase's other schema families creates its own (keys_001.sql,
-- personal_001.sql, tier3_cookies_001.sql all do the same), so this one
-- does too.

CREATE TABLE IF NOT EXISTS schema_version (
    version         INTEGER PRIMARY KEY,
    applied_at      TEXT NOT NULL,
    description     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS view_cache_meta (
    id                              INTEGER PRIMARY KEY CHECK (id = 1),
    share_id                        TEXT NOT NULL,
    source_persona_display_name     TEXT NOT NULL,
    source_persona_type             TEXT NOT NULL,
    status                          TEXT NOT NULL DEFAULT 'active'
                                        CHECK (status IN ('active', 'ended')),
    last_synced_at                  TEXT,
    ended_at                        TEXT
);

CREATE TABLE IF NOT EXISTS view_cache_entities (
    id                          TEXT PRIMARY KEY,
    entity_type                 TEXT NOT NULL,
    display_name                 TEXT NOT NULL,
    aliases                      TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(aliases)),
    parent_entity_id             TEXT,
    status                       TEXT NOT NULL,
    source_url                   TEXT,
    created_at                   TEXT NOT NULL,
    extra_metadata                TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(extra_metadata)),
    redact_identification         INTEGER NOT NULL DEFAULT 0,
    hide_from_shared_surfaces     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS view_cache_entity_facts (
    id                    TEXT PRIMARY KEY,
    entity_id             TEXT REFERENCES view_cache_entities(id) ON DELETE CASCADE,
    field_name            TEXT NOT NULL,
    field_value           TEXT NOT NULL,
    sensitivity           TEXT NOT NULL,
    abstraction_tier2     TEXT NOT NULL,
    abstraction_tier3     TEXT NOT NULL,
    source                TEXT NOT NULL,
    valid_from            TEXT,
    created_at            TEXT NOT NULL,
    extra_metadata        TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(extra_metadata))
);

CREATE INDEX IF NOT EXISTS idx_view_cache_entity_facts_entity
    ON view_cache_entity_facts (entity_id);

CREATE TABLE IF NOT EXISTS view_cache_voice_profile_entries (
    id                 TEXT PRIMARY KEY,
    source_id          TEXT,
    precedence         INTEGER NOT NULL,
    attribute          TEXT NOT NULL,
    value              TEXT NOT NULL,
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL,
    extra_metadata     TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(extra_metadata))
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (1, datetime('now'),
    'items.id=304: view_cache_meta/entities/entity_facts/voice_profile_entries -- VIEW-ONLY persona sharing''s read-only recipient cache');
