-- shared_008.sql
--
-- items.id=302: adds pending_persona_shares.source_persona_type, the one
-- field missing from shared_007.sql's envelope that recipient-side
-- materialization actually needs. persona_store::create_persona() requires
-- a non-null persona_type; the payload built by send_persona_share()
-- (items.id=299) never carried the sender Persona's type, only its
-- display_name. Rather than have accept_persona_share() take persona_type
-- as a caller-supplied parameter (deferring the policy question to a
-- not-yet-built UI), Jason's direction this session was to carry the real
-- value across on the envelope itself, mirroring source_persona_display_name
-- exactly: send_persona_share() now also binds persona.persona_type here.
--
-- NOT NULL DEFAULT 'personal': SQLite requires a DEFAULT to ADD COLUMN NOT
-- NULL to an existing table. The default is never actually relied upon --
-- this is pre-release with zero shipped rows, and every row inserted by
-- send_persona_share() from this point on supplies a real value explicitly.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE pending_persona_shares
    ADD COLUMN source_persona_type TEXT NOT NULL DEFAULT 'personal';

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (8, datetime('now'),
    'items.id=302: pending_persona_shares.source_persona_type -- carries the sender Persona type across the envelope for recipient-side materialization');
