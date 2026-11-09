-- shared_042.sql
--
-- items.id=889 (decisions.id=940 ruling 1, decisions.id=938): AND-guards, a typed
-- clause list on composition rows. Number checked against the live schema
-- directory on the build date: shared_041 was the latest. Earlier files are not
-- edited.
--
-- A composition row may already carry ONE typed guard (shared_039.sql:
-- guard_var, guard_op, guard_value). That single guard is not enough for the
-- Tech Support draft, which needs a row to run only when an intent AND a ticket
-- state AND the write mode all hold. This file adds a LIST of further clauses:
--
--   guard_clauses  NULL, or a JSON array of clause objects. Each object is
--                    {"var": name, "op": op, "value": value}
--                  with the same closed operators as the single guard:
--                    eq, ne       value is one JSON string
--                    in           value is a JSON array of strings
--                    present,
--                    absent       no "value" key
--                  An unknown key, a missing or surplus value, an unknown
--                  operator, an empty array, a duplicate clause and a clause
--                  that is not an object each fail LOAD with their own error.
--
-- THE ROW RUNS ONLY WHEN EVERY CLAUSE HOLDS: the single guard (when set) and
-- every clause in guard_clauses, AND only. The total over both is capped
-- (conductor::guard::MAX_GUARD_CLAUSES, 4). The single guard columns keep
-- working unchanged: a row that sets only them behaves exactly as before, and a
-- row may set both (the single guard is then one more clause).
--
-- All of this is database-only: no YAML syntax, no expression string, and no
-- OR. NULL on every existing row. There is deliberately NO CHECK constraint on
-- guard_clauses: shared.db is unencrypted, so the real gate is the LOAD
-- validator (conductor::guard and the loader in conductor::lifecycle).
--
-- ALTER TABLE ... ADD COLUMN precedent: shared_024.sql, shared_039.sql and
-- shared_041.sql.
--
-- CONTENT CLASS (decisions.id=862): clauses are shipped core Focus content. They
-- hold variable names and closed literals, never a personal value.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE focus_block_compositions ADD COLUMN guard_clauses TEXT;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (42, datetime('now'),
    'items.id=889: guard_clauses on focus_block_compositions (AND-guards, decisions.id=940 ruling 1) -- NULL on existing rows');
