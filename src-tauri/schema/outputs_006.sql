-- outputs_006.sql
--
-- items.id=416 (decisions.id=767) -- Privacy Guardian classification
-- established at creation time, not deferred to whenever a later action
-- first triggers it. Prior to this, output_scan::scan_output
-- (ScanIntensity::Full) only ever ran lazily, inside
-- commands::library::prepare_clipboard_copy, re-scanning identical
-- (immutable, per output_store.rs's own header note -- the only mutation
-- path is the delete-time content-zeroing UPDATE) content on every single
-- copy. lifecycle.rs::output() now runs the real scan once, at creation,
-- and these columns cache that result.
--
-- FIVE ADDITIVE COLUMNS, deliberately separate from the existing
-- `sensitivity`/`sensitivity_severity` pair (outputs_001.sql): those remain
-- an untouched, earlier, in-run classification (FocusRun::output_sensitivity)
-- used elsewhere -- OutputScanResult's shape (blocked/timed_out/findings/
-- plain_language) does not match VALID_SENSITIVITY's four-value enum, so
-- overwriting `sensitivity` with it would silently change its meaning for
-- every existing reader. This is new, additive state instead.
--
--   pg_scan_blocked / pg_scan_timed_out: OutputScanResult's own booleans,
--     stored as INTEGER (0/1) per this codebase's existing convention
--     (is_fast_lane, is_quick_ask, etc., outputs_001.sql).
--
--   pg_scan_plain_language: OutputScanResult.plain_language, nullable.
--
--   pg_scan_findings_json: OutputScanResult.findings (Vec<OutputScanFinding>)
--     serialized as a JSON array -- same JSON-in-TEXT convention already
--     used elsewhere in this codebase (e.g. entities.aliases). NULL when
--     never scanned; '[]' is a real scanned-and-clean result, distinct from
--     NULL's "unknown" via pg_scan_completed_at below, not via this column
--     alone.
--
--   pg_scan_completed_at: NULL is the load-bearing signal here -- it means
--     "never scanned" (a row that predates this migration, or one created
--     via a path that doesn't call scan_output, e.g. document ingestion's
--     save_ingested_output, which is untouched by this item's scope). A
--     bare ALTER TABLE ... DEFAULT 0 on pg_scan_blocked alone, with no way
--     to distinguish "scanned, found nothing" from "never scanned", would
--     make every pre-existing row silently read as safe -- this column is
--     what commands::library::prepare_clipboard_copy checks before trusting
--     pg_scan_blocked as cache instead of falling back to a live scan.
--
-- SQLite CAN add columns with ALTER TABLE ADD COLUMN without a full rebuild
-- -- these are pure additive, nullable-or-defaulted columns; no existing row
-- loses data.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE outputs
    ADD COLUMN pg_scan_blocked INTEGER NOT NULL DEFAULT 0 CHECK (pg_scan_blocked IN (0, 1));

ALTER TABLE outputs
    ADD COLUMN pg_scan_timed_out INTEGER NOT NULL DEFAULT 0 CHECK (pg_scan_timed_out IN (0, 1));

ALTER TABLE outputs
    ADD COLUMN pg_scan_plain_language TEXT;

ALTER TABLE outputs
    ADD COLUMN pg_scan_findings_json TEXT;

ALTER TABLE outputs
    ADD COLUMN pg_scan_completed_at TEXT;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (6, datetime('now'),
    'items.id=416 (decisions.id=767): outputs.pg_scan_blocked/pg_scan_timed_out/pg_scan_plain_language/pg_scan_findings_json/pg_scan_completed_at -- creation-time Privacy Guardian scan cache');
