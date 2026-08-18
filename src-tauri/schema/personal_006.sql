-- personal_006.sql
--
-- items.id=296: group_fact_sources -- persona-level opt-in recording which
-- group_ids this persona's context assembly checks for group facts
-- (GROUP_DB_DESIGN_20260802.md Section 3.2: "a user decides, per Persona,
-- whether that Persona's context assembly should include a given group's
-- facts at all -- a one-time-per-persona opt-in decision, not something
-- resolved dynamically on every Focus run").
--
-- PLACEMENT: personal.db, not shared.db. Direct precedent: group_keys
-- (personal_005.sql) already records per-persona group-scoped state here,
-- for the same reason -- personal.db is already one file per
-- (user_id, persona_id) pair, and is already opened by
-- lifecycle.rs::build_personal_track() on every Focus run (it's where
-- entity_facts is loaded from). Putting the opt-in table here means that
-- read costs zero new DB connections. shared.db's own group tables
-- (pending_group_invitations, shared_003.sql; user_sharing_keys,
-- shared_004.sql) are unencrypted and exist to be readable BEFORE a
-- persona's key is unlocked -- that constraint doesn't apply here: this
-- table is only ever read from inside an already-authorized Focus run.
--
-- NO persona_id COLUMN: same convention every table in this file already
-- follows (personal_001.sql's own header) -- the file's own path already
-- encodes (user_id, persona_id).
--
-- group_id is the PRIMARY KEY, not a surrogate id column -- one opt-in
-- decision per group per persona is the entire invariant this table holds,
-- matching group_keys' own (group_id PRIMARY KEY, no persona_id column)
-- shape exactly.
--
-- NO FK to group_keys(group_id): SQLite cannot enforce a foreign key
-- across separate database files, and opt-in status is deliberately
-- independent of key residency -- Section 3.3 point 4 treats the opt-in
-- table and the resident-key check (auth::registry::GroupKeyRegistry) as
-- two separate, unstacked gates. A persona can be opted into a group
-- whose key isn't currently resident (build_personal_track() simply skips
-- it that run); enforcing referential integrity here would conflate the
-- two checks the design doc explicitly keeps apart.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS group_fact_sources (
    group_id        TEXT NOT NULL PRIMARY KEY,
    opted_in_at     TEXT NOT NULL
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (6, datetime('now'),
    'items.id=296: group_fact_sources -- per-persona opt-in for which group facts context assembly checks');
