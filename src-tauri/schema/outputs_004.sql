-- outputs_004.sql
--
-- decisions.id=486 (D6-444): ingested-document storage model (items.id=383).
-- Ingested documents live in this same outputs table as QR-generated
-- content, distinguished by `source`, excluded from the default Library
-- view (source='qr_generated'), surfaced only in a separate Imported view
-- (source='external_ingested') -- view routing enforced at the command
-- layer (commands/library.rs::list_outputs), not here.
--
-- SIX ADDITIVE COLUMNS:
--
--   source: qr_generated (default, existing rows) | external_ingested.
--
--   project_entity_id: nullable, no FK. Optional Focus Project linkage
--     (decisions.id=417/448) -- entities live in personal.db, a different
--     encrypted database file, so a real FK is not possible here. Matches
--     focus_id's own no-FK precedent on focus_runs (outputs_001.sql).
--
--   focus_slug: nullable. For an ingested row this carries the REAL Focus
--     the user filed the document under. Needed because the ingest-only
--     focus_run created at upload time (persistence/output_store.rs::
--     create_ingest_focus_run) uses a fixed system pseudo-Focus id
--     ("system-ingest") for focus_runs.focus_id -- not the real target
--     Focus -- so real-Focus association has to be carried here instead of
--     relying on the usual focus_runs join. NULL for qr_generated rows
--     (real Focus already reachable via the existing focus_run_id join) and
--     for an ingested document not filed under any specific Focus.
--
--   storage_path: nullable. Path to the current version's ChaCha20Poly1305-
--     encrypted blob on disk (persistence/ingest_blob.rs), for real uploaded
--     files that cannot live in the `content` TEXT column. NULL for
--     ordinary qr_generated/text outputs, which are unaffected by this
--     migration and keep using `content` exactly as before.
--
--   storage_version: current version number of the file at storage_path.
--     Writing an edited version bumps this and repoints storage_path at a
--     new file; prior version files are retained on disk, not deleted --
--     this is the edit/versioning support the bytes-vs-path resolution
--     requires. Defaults to 1 for both new ingested rows and (vacuously)
--     existing qr_generated rows, which never read this column.
--
--   original_filename: nullable. Display name for the uploaded file --
--     kept in the DB rather than the on-disk filename itself, since the
--     blob's own filename is a version-numbered path segment, not the
--     original name.
--
-- CHECK on `source` is the only new constraint; the other five columns
-- carry no CHECK -- their valid-value shape (or lack of one) is enforced at
-- the application layer (persistence/output_store.rs), same convention as
-- outputs_002.sql's `source` column on extract_confirm_candidates (an
-- unrelated column of the same name on a different table -- see items.id=383
-- for the naming-collision history this caused).
--
-- SQLite CAN add columns with ALTER TABLE ADD COLUMN without a full rebuild
-- -- these are pure additive, nullable-or-defaulted columns; no existing row
-- loses data.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE outputs
    ADD COLUMN source TEXT NOT NULL DEFAULT 'qr_generated'
        CHECK (source IN ('qr_generated', 'external_ingested'));

ALTER TABLE outputs
    ADD COLUMN project_entity_id TEXT;

ALTER TABLE outputs
    ADD COLUMN focus_slug TEXT;

ALTER TABLE outputs
    ADD COLUMN storage_path TEXT;

ALTER TABLE outputs
    ADD COLUMN storage_version INTEGER NOT NULL DEFAULT 1;

ALTER TABLE outputs
    ADD COLUMN original_filename TEXT;

CREATE INDEX IF NOT EXISTS idx_outputs_source
    ON outputs (source, created_at DESC)
    WHERE status = 'active';

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (4, datetime('now'),
    'decisions.id=486 (items.id=383): outputs.source/project_entity_id/focus_slug/storage_path/storage_version/original_filename for ingested-document storage model');
