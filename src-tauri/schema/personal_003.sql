-- personal_003.sql
--
-- decisions.id=513 (D6-471) foundation, entity-first per the decision's own
-- sequencing: object-level visibility and display control flags, first
-- implemented at entity record level. items.id=175.
--
-- TWO NEW COLUMNS on entities:
--   redact_identification      bool, default false
--   hide_from_shared_surfaces  bool, default false
--
-- Full flag semantics live in decisions.id=513, not restated here (P4 —
-- One Home).
--
-- BOOLEAN STORAGE: matches the existing convention on entity_facts.
-- cross_persona_export (personal_001.sql) — INTEGER NOT NULL DEFAULT 0,
-- CHECK'd to exactly 0 or 1.
--
-- NO INDEX: cross_persona_export, the one existing analogous boolean flag
-- in this schema family, has no index — read via full scan. No verified
-- query path yet needs an index on either new column here; add one later
-- if and when a real read path demonstrates the need, rather than
-- pre-building against a guess.
--
-- SQLite CAN add columns with ALTER TABLE ADD COLUMN without a full rebuild
-- — unlike personal_002.sql's status CHECK widening, no PRAGMA
-- legacy_alter_table dance is needed here. New columns with a DEFAULT are a
-- pure additive change; no existing row loses data, no existing reference
-- (entity_facts.entity_id) is disturbed.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals — parse_statements() is not a general-purpose SQL parser.

ALTER TABLE entities
    ADD COLUMN redact_identification INTEGER NOT NULL DEFAULT 0
        CHECK (redact_identification IN (0, 1));

ALTER TABLE entities
    ADD COLUMN hide_from_shared_surfaces INTEGER NOT NULL DEFAULT 0
        CHECK (hide_from_shared_surfaces IN (0, 1));

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (3, datetime('now'),
    'decisions.id=513 entity-level visibility flags (items.id=175): entities.redact_identification, entities.hide_from_shared_surfaces');
