-- shared_017.sql
--
-- items.id=221 -- drops context_groups and context_group_members, an early
-- Release-1 placeholder for a generic multi-user grouping primitive
-- (shared_001.sql). Fully superseded by group.db and household admin/
-- sharing, which solve the same need with a more specific, actually-
-- implemented design. Confirmed zero Rust code references anywhere in
-- src-tauri/src (2026-08-03, re-confirmed 2026-09-08), and zero write code
-- paths ever existed for either table in this project's history -- current
-- Rust source and the archived retired Python backend both show only
-- schema-mechanics (CREATE TABLE / rebuild-copy) statements referencing
-- them, never an application-level INSERT. Approved by Jason 2026-08-03.
--
-- WHY A NEW VERSIONED FILE, NOT AN IN-PLACE shared_001.sql EDIT: unlike
-- this project's "pre-release, zero shipped users" consolidation precedent
-- (used for shared_001.sql itself, Session: Chat-DEV 2026-07-24), this
-- project now has real installed databases (local dev instance, NAS
-- mirror) that already ran shared_001.sql's CREATE TABLE IF NOT EXISTS for
-- both tables -- removing the statements from shared_001.sql would stop
-- new databases from creating them but would not drop them from any
-- already-installed database, since v1 files are re-run every startup but
-- never retroactively affect tables an earlier run already created.
-- validate_v1_rerun_safety() (migrations.rs) also forbids DROP/ALTER in v1
-- outright. Same precedent as tier3_providers -> providers (shared_013.sql)
-- and focus_settings_friction_decisions' rebuild (shared_011.sql): a real
-- migration file, run exactly once, is the only way to remove something
-- already shipped. shared_001.sql's own CREATE TABLE statements for these
-- two tables are left untouched as the historical record of what v1
-- created, per that same precedent.
--
-- FK dependency order: context_group_members (child, FK to context_groups)
-- dropped before context_groups (parent).
--
-- Plain DROP TABLE, not IF EXISTS: both tables are unconditionally created
-- by shared_001.sql, so every database that reaches this migration is
-- guaranteed to have them (same reasoning as shared_013.sql's
-- `DROP TABLE tier3_providers`).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

DROP TABLE context_group_members;

DROP TABLE context_groups;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (17, datetime('now'),
    'items.id=221: drop unused context_groups/context_group_members tables (Release-1 placeholder, zero code references, superseded by group.db + household admin/sharing)');
