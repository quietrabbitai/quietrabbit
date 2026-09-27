-- shared_022.sql
--
-- items.id=52 Part 1: nightly batch runner infrastructure. Two new
-- instance_config rows (shared_001.sql's existing generic key/value table,
-- already holding auth_lockout_enabled/role_enforcement/instance_name) so
-- conductor::nightly_batch can read a configurable fire hour and persist
-- its own last-run wall-clock timestamp, the same "config as data, not a
-- new table" idiom those existing rows already establish.
--
-- nightly_batch_hour: 0-23, local time, default 2 (2am). Range validation
-- happens in Rust at read time (conductor::nightly_batch), not via a CHECK
-- constraint here -- this table has no per-key CHECK precedent to match.
--
-- nightly_batch_last_run_at: RFC3339 UTC timestamp of the last completed
-- sweep, or '' meaning "never run" -- same empty-string sentinel
-- instance_name's default already uses.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

INSERT OR IGNORE INTO instance_config VALUES ('nightly_batch_hour', '2');
INSERT OR IGNORE INTO instance_config VALUES ('nightly_batch_last_run_at', '');

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (22, datetime('now'),
    'items.id=52 Part 1: instance_config rows for nightly batch runner schedule (nightly_batch_hour, nightly_batch_last_run_at)');
