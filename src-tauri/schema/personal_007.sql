-- personal_007.sql
--
-- items.id=306: local-edit protection for voice_profiles, closing the gap
-- items.id=303's own module header (persona_sync/engine.rs, SCOPE section)
-- flagged: "voice_profiles has no modification_state column at all -- an
-- incoming entry always overwrites by id (upsert), with no local-edit
-- protection possible for this table today."
--
-- SAME THREE STATES AS entities (personal_002.sql, decisions.id=502),
-- same meanings, same default:
--   pristine       came from a source/sync, unedited -- future sync updates
--                  auto-apply
--   user_modified  locally edited -- future sync surfaces a conflict instead
--                  of silently overwriting
--   user_created   QR-native, no import/sync origin -- sync never touches it
-- Default is user_created, matching entities' own reasoning: a row with no
-- sync origin is QR's own.
--
-- WHY NO source_registry_id COLUMN, unlike entities: entities needs it for
-- per-source tombstoning (deletion-by-absence) and source-scoped
-- pending_refresh status. Neither applies to voice_profiles in this item's
-- scope -- voice_profiles deletion-by-absence is an explicitly out-of-scope
-- gap of the same shape as the existing, already-flagged entity_facts one.
-- Provisioning (persona_sync::engine::provision_sync_relationship) scopes
-- correctly by persona_id + modification_state alone; adding an unused
-- column now would be schema-first speculation.
--
-- WHY A PLAIN ADD COLUMN, not a rebuild: unlike personal_002.sql, this adds
-- a new column rather than widening a CHECK on an existing one -- the same
-- shape personal_003.sql already used for entities.redact_identification /
-- hide_from_shared_surfaces. SQLite supports ADD COLUMN with a CHECK
-- constraint directly given a constant DEFAULT; no PRAGMA legacy_alter_table
-- dance needed.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE voice_profiles
    ADD COLUMN modification_state TEXT NOT NULL DEFAULT 'user_created'
        CHECK (modification_state IN ('pristine', 'user_modified', 'user_created'));

CREATE INDEX IF NOT EXISTS idx_voice_profiles_modification_state
    ON voice_profiles (modification_state);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (7, datetime('now'),
    'items.id=306: voice_profiles.modification_state -- local-edit protection for synced voice profile entries, mirroring entities/decisions.id=502');
