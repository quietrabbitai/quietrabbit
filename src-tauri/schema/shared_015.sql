-- shared_015.sql
--
-- items.id=433: drops the legacy users.tier2_provider_preference column
-- outright. items.id=432 already repointed lifecycle.rs's provider
-- resolution at user_provider_preference_store::find_preferred_provider()
-- (shared_013.sql) -- this column has not been read by any code path since
-- that change landed (commands/tier2.rs's set_tier2_provider_preference
-- doc comment named this column "kept populated, not read by anything
-- anymore, until items.id=433 drops it"). Confirmed no remaining reads
-- this session; the corresponding dead read/write helpers in
-- auth/user_store.rs and the dual-write call in commands/tier2.rs are
-- removed in the same change that adds this migration.
--
-- SQLite's ALTER TABLE DROP COLUMN supports dropping a column that carries
-- its own single-column CHECK constraint (verified against this project's
-- linked SQLite version) -- no table-rebuild needed.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE users DROP COLUMN tier2_provider_preference;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (15, datetime('now'),
    'items.id=433: drop dead users.tier2_provider_preference column (superseded by user_provider_preference, items.id=428/432)');
