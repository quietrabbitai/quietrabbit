
-- shared_041.sql
--
-- items.id=887 (decisions.id=939 section 4, decisions.id=938): typed store_op
-- columns on composition rows. Number checked against the live schema
-- directory on the build date: shared_040 was the latest. Earlier files are
-- not edited.
--
-- A composition row may be a STORE WRITER: instead of running a model step it
-- calls ONE typed store function of the persona's outputs.db (the session
-- variable store and the case store, items.id=881). Like the guard columns
-- (shared_039.sql) the language is closed and typed: no expression string, no
-- YAML syntax, database-only, authors are shipped content only.
--
--   store_op      the operation, one of a CLOSED set (conductor::store_op):
--                 latch, session_put, case_open, case_update, strategy_append,
--                 delta_record, delta_confirm, delta_changed
--   store_target  a NAME the operation writes to or publishes under:
--                 session_put  the session variable (a bound name with
--                              source session, never write_mode)
--                 case_open, delta_record  the pass variable the new case id
--                              or delta id is published under (optional)
--                 every other operation  NULL
--   store_args    a JSON object {slot: variable_name}: for each argument slot
--                 of the operation, the NAME of the variable that supplies it
--                 (an earlier row's output variable or published id, or a
--                 session-bound name). Never a literal and never an
--                 expression. NULL for latch.
--
-- All three are NULL on a row that is not a store writer, which is every
-- existing row. A row with store_op set must have no block_stable_id, no
-- schedule trigger and an empty customization.
--
-- There is deliberately NO CHECK constraint on store_op: shared.db is
-- unencrypted, so the real gate is the LOAD validator (conductor::store_op and
-- the loader in conductor::lifecycle), which gives an unknown operation, a
-- half-set row, a bad slot and an unreachable variable each their own error.
-- A guard (shared_039.sql) still applies to a store writer: a row whose guard
-- is false writes nothing.
--
-- ALTER TABLE ... ADD COLUMN precedent: shared_024.sql and shared_039.sql.
--
-- CONTENT CLASS (decisions.id=862): store_op rows are shipped core Focus
-- content. They hold variable NAMES only, never a personal value.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE focus_block_compositions ADD COLUMN store_op TEXT;
ALTER TABLE focus_block_compositions ADD COLUMN store_target TEXT;
ALTER TABLE focus_block_compositions ADD COLUMN store_args TEXT;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (41, datetime('now'),
    'items.id=887: store_op, store_target and store_args on focus_block_compositions (typed store writers, decisions.id=939 section 4) -- all NULL on existing rows');
