-- personal_012.sql
--
-- items.id=930 stage 3 (decisions.id=953 rulings 2 and 6): the user's per-field
-- DISCLOSURE POLICY for the closed catalogue of personal facts that may reach an
-- external step (block_data/personal_fact_fields.json, conductor::personal_fact_fields).
-- Number checked against the live schema directory on the build date: personal_011
-- was the latest. Numbers before the items.id=633 cutover are provisional, since
-- the cutover folds every prefix into one baseline.
--
-- WHY A TABLE AND NOT THE entity_facts COLUMNS. entity_facts.abstraction_tier2 and
-- abstraction_tier3 default to pass and are indistinguishable from a choice, so the
-- disclosure bridge (items.id=931) never reads them. This table holds only what the
-- user chose.
--
-- One row per catalogued field name. tier2 and tier3 are the user's choice for that
-- external tier: pass, summarize, omit or the reserved ask. NULL means the user made
-- no choice for that tier and the catalogue default applies (omit at both tiers for
-- every v1 field), so a choice for one tier never invents one for the other. There is
-- deliberately NO CHECK on the two columns: the writer (persistence/
-- fact_policy_store.rs) validates against the catalogue and the hard floors, and a
-- reader treats a value it cannot parse as omit (fail closed, decisions.id=953 ruling
-- 10), which a CHECK would make untestable and would not make safer.
-- not_permitted is system-only and is never stored.
--
--   field_name   a catalogue name (the primary key). Names are never user content.
--   tier2        the choice for tier 2, or NULL.
--   tier3        the choice for tier 3, or NULL.
--   set_at       when either tier was last set (UTC, ISO 8601).
--   set_by       who set it: user (the only value in R1).
--
-- No foreign keys, no triggers, no value text: nothing here is user content.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS fact_disclosure_policy (
    field_name  TEXT PRIMARY KEY,
    tier2       TEXT,
    tier3       TEXT,
    set_at      TEXT NOT NULL,
    set_by      TEXT NOT NULL
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (12, datetime('now'),
    'items.id=930 (decisions.id=953): fact_disclosure_policy, the per-field disclosure choices for catalogued personal facts in the encrypted personal.db');
