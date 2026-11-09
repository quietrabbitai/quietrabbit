-- shared_039.sql
--
-- items.id=878 (decisions.id=938, G1): guard columns on composition rows.
-- Number checked against the live schema directory on the build date:
-- shared_038 was the latest. shared_021 is not edited.
--
-- A composition row may carry a typed guard. A row whose guard is false is
-- SKIPPED at run time, so row order and UNIQUE(focus_id, sequence_index) stay.
-- The condition language is closed and typed: no expression string, and no YAML
-- syntax (database-only, authors are shipped content only).
--
--   guard_var    name of an EARLIER row's output variable
--   guard_op     eq, ne, in, present or absent
--   guard_value  eq and ne: one literal. in: a JSON array of literals.
--                present and absent: NULL.
--
-- All three are NULL on an unguarded row, which is every existing row. There is
-- deliberately NO CHECK constraint on guard_op: shared.db is unencrypted, so the
-- real gate is the LOAD validator (conductor::guard and the loader in
-- conductor::lifecycle), which gives an unknown operator, a half-set guard and a
-- malformed value each their own error.
--
-- ALTER TABLE ... ADD COLUMN precedent: shared_024.sql.
--
-- CONTENT CLASS (decisions.id=862): guards are shipped core Focus content. They
-- hold no personal values.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE focus_block_compositions ADD COLUMN guard_var TEXT;
ALTER TABLE focus_block_compositions ADD COLUMN guard_op TEXT;
ALTER TABLE focus_block_compositions ADD COLUMN guard_value TEXT;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (39, datetime('now'),
    'items.id=878: guard_var, guard_op and guard_value on focus_block_compositions (G1, decisions.id=938) -- all NULL on existing rows');
