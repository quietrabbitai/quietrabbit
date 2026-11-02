-- personal_009.sql
--
-- items.id=661 (decisions.id=862): two things that used to sit in plaintext
-- shared.db move into the per-Persona encrypted personal.db.
--
-- persona_floor_consent: the Persona's saved floor consent preference
-- (D5-152). It was personas.extra_metadata.floor_consent_preference in
-- shared.db. A single row (id is pinned to 1): personal.db is already one
-- file per user and Persona, so there is no persona_id column (personal_001.sql
-- header). mode is "modified" or "local" as the Conductor reads it,
-- abstraction_tier is set for "modified", consent_version is the schema
-- version of the preference ("1" today). Follows pf_standing_preferences
-- (personal_008.sql), which is in this file for the same reason.
--
-- group_keys.group_display_name: the group's name. Until now the only record
-- of it was pending_group_invitations.group_display_name in plaintext
-- shared.db, including the creator's own self-accepted row. The name is
-- written when the key is saved (create_group, accept_invitation), so the
-- Persona's own encrypted file is its durable home. DEFAULT '' keeps rows
-- written before this migration valid.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS persona_floor_consent (
    id                  INTEGER PRIMARY KEY CHECK (id = 1),
    mode                TEXT NOT NULL,
    abstraction_tier    INTEGER,
    consent_timestamp   TEXT NOT NULL,
    consent_version     TEXT NOT NULL
);

ALTER TABLE group_keys ADD COLUMN group_display_name TEXT NOT NULL DEFAULT '';

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (9, datetime('now'),
    'items.id=661 (decisions.id=862): persona_floor_consent (moved from plaintext personas.extra_metadata) and group_keys.group_display_name (the group name, moved from plaintext pending_group_invitations)');
