-- persistence/schema/personal_002.sql
-- Layer 6: ADR-012 Tier De-coupling — disclosure_log schema update
-- Adds execution_tier and abstraction_tier columns to disclosure_log.
--
-- Both columns are nullable to preserve compatibility with Layer 1-5
-- records written before the split. All new records (Layer 6+) will
-- always have both values populated.
--
-- execution_tier:  model capability ceiling — which inference tier ran
-- abstraction_tier: Gate1 field policy tier — how data was shaped
--
-- These replace the single routing_tier column's semantic overloading.
-- routing_tier is preserved for historical records but deprecated
-- for all records written after Layer 6 migration.
--
-- Migration runner: run_migrations() via migrate_personal_db()
-- Atomicity: wrapped in SAVEPOINT by the migration runner (no explicit
-- BEGIN/COMMIT here — executescript() is never used, per CLAUDE.md)
--
-- SCHEMA AUTHORING RULE: no semicolons inside string literals.

ALTER TABLE disclosure_log ADD COLUMN execution_tier INTEGER;

ALTER TABLE disclosure_log ADD COLUMN abstraction_tier INTEGER;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (2, datetime('now'), 'ADR-012: add execution_tier and abstraction_tier to disclosure_log');
