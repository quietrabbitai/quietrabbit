-- outputs_012.sql
--
-- items.id=809 -- persist OutputScanResult.pf_error so a creation-time scan
-- that FAILED can be told apart from a block caused by real findings.
--
-- Until now output_store persisted blocked and timed_out only
-- (items.id=613 left pf_error out to avoid a migration). A Privacy Filter
-- error (items.id=613) or an unavailable filter in a release build
-- (items.id=618) therefore cached a permanent block that looked exactly like
-- a real-findings block, and commands::library::prepare_clipboard_copy never
-- re-scanned it. With this column the cache is usable only when the scan
-- actually completed: pg_scan_completed_at is set and neither pg_scan_timed_out
-- nor pg_scan_pf_error is set. A failed scan is a cache miss, re-scanned live
-- on the next copy and backfilled (backfill_scan_result).
--
-- WHY A NEW FILE, NOT AN EDIT OF outputs_006/007: only v1 schema files are
-- re-run on every startup (migrations.rs run_pending), and they may not contain
-- ALTER TABLE. outputs_006/007 are applied exactly once, so amending them in
-- place would add the column to fresh installs only and leave every existing
-- database without it. decisions.id=857 consolidates the outputs prefix into
-- one baseline before first ship (items.id=633), which absorbs this file.
--
-- No backfill of existing rows: rows blocked before this column existed read as
-- pf_error = 0. Existing databases are development data, reset at the
-- decisions.id=857 cutover.
--
-- Plain additive column, no table rebuild, so this version is not in
-- FK_OFF_MIGRATIONS.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE outputs
    ADD COLUMN pg_scan_pf_error INTEGER NOT NULL DEFAULT 0 CHECK (pg_scan_pf_error IN (0, 1));

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (12, datetime('now'),
    'items.id=809: outputs.pg_scan_pf_error -- a creation-time scan that failed (Privacy Filter error or unavailable) is a cache miss and is re-scanned on the next copy');
