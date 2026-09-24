-- outputs_007.sql
--
-- items.id=559 -- foundation for decisions.id=421 (four-state document
-- lifecycle) and decisions.id=422 (document relationship model), both
-- locked 2026-06-27, never built until now.
--
-- decisions.id=421: outputs.status becomes draft / finalized /
-- potentially-stale / archived, replacing the original two-state
-- active/deleted CHECK (outputs_001.sql). Transitions (draft->finalized on
-- session completion, finalized->potentially-stale on external export
-- without return, potentially-stale->finalized on import_external_draft
-- completion, any->archived user-initiated and reversible only by explicit
-- restore) are NOT implemented here -- schema/foundation only, no
-- application state-machine logic.
--
-- decisions.id=422: document_relationship (prime/update/fork/reference/
-- continue_draft), parent_output_id, superseded_by added to outputs.
--
-- SOFT-DELETE DESIGN NOTE: 'deleted' is not one of decisions.id=421's four
-- states -- it never named deletion at all. Folding it in as a 5th status
-- value would mean every future lifecycle-transition site has to except it,
-- and would destroy the fact that a deleted row had a real prior lifecycle
-- state (the old delete_output_conn's `status = 'deleted'` UPDATE
-- overwrote that unconditionally). 'archived' is not a substitute either --
-- it is explicitly reversible via restore (decisions.id=421's own wording),
-- while delete destroys content (zeros it) and is not advertised as
-- reversible anywhere in delete_output's doc comment. Soft-delete instead
-- gets its own nullable deleted_at TEXT timestamp, following this table's
-- own existing convention for lifecycle events (purge_scheduled_at /
-- purge_attempted_at / purged_at, outputs_001.sql) rather than inventing a
-- new pattern. delete_output_conn (output_store.rs) is updated in this same
-- change to set deleted_at instead of mutating status.
--
-- WHY A TABLE REBUILD, NOT ALTER TABLE ADD COLUMN: SQLite has no ALTER
-- COLUMN to swap a CHECK constraint (same constraint shared_020.sql hit
-- rebuilding focus_settings). Runs inside run_pending's per-version
-- SAVEPOINT (migrations.rs), atomic with the rest of this file.
--
-- MIGRATION APPROACH FOR EXISTING ROWS: value-mapped copy, not a
-- destructive drop (matches shared_020.sql's precedent of preserving real
-- rows whenever the mapping is well-defined):
--   status='active'  -> status='finalized' (the old 2-state model never
--     tracked finalization, so there is no way to recover which of the 4
--     states a row "should" be -- 'finalized' is the one that preserves
--     current effective behavior: every existing active row is already
--     being treated by list_outputs/get_output as eligible for voice
--     calibration/content continuity, exactly what decisions.id=421 defines
--     'finalized' to mean).
--   status='deleted' -> status='archived', deleted_at backfilled from the
--     row's own updated_at (the timestamp the old delete path already
--     stamped when it flipped status to 'deleted').
--   document_relationship='prime', parent_output_id=NULL, superseded_by=NULL
--     for every existing row -- the old schema had zero provenance
--     tracking, so every pre-existing row is, by definition, an independent
--     document with no recorded parent.
--
-- FTS5 REBUILD IS MANDATORY, NOT OPTIONAL: outputs_fts is an external-
-- content FTS5 table keyed on outputs.rowid (content='outputs',
-- content_rowid='rowid', outputs_001.sql). DROP TABLE outputs + RENAME
-- outputs_new TO outputs reassigns fresh sequential rowids to every row on
-- the INSERT ... SELECT below, silently desyncing the FTS shadow index from
-- the new table's real rowids unless explicitly rebuilt. DROP TABLE also
-- drops the three outputs_fts_insert/update/delete triggers (defined ON
-- outputs), so they are recreated verbatim below, followed by
-- INSERT INTO outputs_fts(outputs_fts) VALUES('rebuild') to force a full
-- resync.
--
-- sensitivity_severity is a GENERATED ALWAYS ... STORED column -- excluded
-- from both the INSERT column list and the SELECT list below; SQLite
-- recomputes it automatically (same as save_output's own doc comment notes
-- for the original INSERT path).
--
-- Self-reference-to-self guards on parent_output_id/superseded_by match
-- personal_002.sql's entities.parent_entity_id != id precedent. Both
-- columns REFERENCE outputs(id) -- the old table's name -- during
-- outputs_new's own CREATE TABLE, same as personal_002.sql's
-- entities_v2.parent_entity_id REFERENCES entities(id): SQLite resolves the
-- REFERENCES target by name when enforced, not to a physical table at
-- creation time, so this resolves correctly once outputs_new is renamed to
-- outputs below.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE outputs_new (
    id                      TEXT PRIMARY KEY,
    focus_run_id            TEXT NOT NULL REFERENCES focus_runs(id),
    output_type             TEXT NOT NULL,
    content                 TEXT,
    content_pre_validation  TEXT,
    content_post_validation TEXT,
    sensitivity             TEXT NOT NULL DEFAULT 'general'
                                CHECK (sensitivity IN
                                    ('general','personal','medical','financial')),
    sensitivity_severity    INTEGER NOT NULL GENERATED ALWAYS AS (
                                CASE sensitivity
                                    WHEN 'general'   THEN 1
                                    WHEN 'personal'  THEN 2
                                    WHEN 'medical'   THEN 3
                                    WHEN 'financial' THEN 4
                                    ELSE 99
                                END
                            ) STORED,
    validation_provider     TEXT,
    validation_delta        TEXT,
    quality_rating          INTEGER,
    status                  TEXT NOT NULL DEFAULT 'draft'
                                CHECK (status IN
                                    ('draft','finalized','potentially-stale','archived')),
    deleted_at              TEXT,
    document_relationship   TEXT NOT NULL DEFAULT 'prime'
                                CHECK (document_relationship IN
                                    ('prime','update','fork','reference','continue_draft')),
    parent_output_id        TEXT REFERENCES outputs(id) ON DELETE SET NULL
                                CHECK (parent_output_id IS NULL
                                       OR parent_output_id != id),
    superseded_by           TEXT REFERENCES outputs(id) ON DELETE SET NULL
                                CHECK (superseded_by IS NULL
                                       OR superseded_by != id),
    created_at              TEXT NOT NULL,
    updated_at              TEXT NOT NULL,
    purge_scheduled_at      TEXT,
    purge_attempted_at      TEXT,
    purge_attempts          INTEGER NOT NULL DEFAULT 0,
    purged_at               TEXT,
    extra_metadata          TEXT NOT NULL DEFAULT '{}',
    source                  TEXT NOT NULL DEFAULT 'qr_generated'
                                CHECK (source IN ('qr_generated', 'external_ingested')),
    project_entity_id       TEXT,
    focus_slug              TEXT,
    storage_path            TEXT,
    storage_version         INTEGER NOT NULL DEFAULT 1,
    original_filename       TEXT,
    pg_scan_blocked         INTEGER NOT NULL DEFAULT 0 CHECK (pg_scan_blocked IN (0, 1)),
    pg_scan_timed_out       INTEGER NOT NULL DEFAULT 0 CHECK (pg_scan_timed_out IN (0, 1)),
    pg_scan_plain_language  TEXT,
    pg_scan_findings_json   TEXT,
    pg_scan_completed_at    TEXT
);

INSERT INTO outputs_new
    (id, focus_run_id, output_type, content, content_pre_validation,
     content_post_validation, sensitivity, validation_provider,
     validation_delta, quality_rating, status, deleted_at,
     document_relationship, parent_output_id, superseded_by,
     created_at, updated_at, purge_scheduled_at, purge_attempted_at,
     purge_attempts, purged_at, extra_metadata, source, project_entity_id,
     focus_slug, storage_path, storage_version, original_filename,
     pg_scan_blocked, pg_scan_timed_out, pg_scan_plain_language,
     pg_scan_findings_json, pg_scan_completed_at)
SELECT
    id, focus_run_id, output_type, content, content_pre_validation,
    content_post_validation, sensitivity, validation_provider,
    validation_delta, quality_rating,
    CASE status
        WHEN 'active'  THEN 'finalized'
        WHEN 'deleted' THEN 'archived'
    END,
    CASE status WHEN 'deleted' THEN updated_at ELSE NULL END,
    'prime', NULL, NULL,
    created_at, updated_at, purge_scheduled_at, purge_attempted_at,
    purge_attempts, purged_at, extra_metadata, source, project_entity_id,
    focus_slug, storage_path, storage_version, original_filename,
    pg_scan_blocked, pg_scan_timed_out, pg_scan_plain_language,
    pg_scan_findings_json, pg_scan_completed_at
FROM outputs;

DROP TABLE outputs;

ALTER TABLE outputs_new RENAME TO outputs;

CREATE INDEX IF NOT EXISTS idx_outputs_focus_run
    ON outputs (focus_run_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_outputs_not_deleted
    ON outputs (deleted_at, created_at DESC)
    WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_outputs_source
    ON outputs (source, created_at DESC)
    WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_outputs_parent
    ON outputs (parent_output_id) WHERE parent_output_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_outputs_superseded_by
    ON outputs (superseded_by) WHERE superseded_by IS NOT NULL;

CREATE TRIGGER IF NOT EXISTS outputs_fts_insert
    AFTER INSERT ON outputs BEGIN
    INSERT INTO outputs_fts(rowid, content, output_type)
    VALUES (new.rowid, COALESCE(new.content,''), COALESCE(new.output_type,''));
END;

CREATE TRIGGER IF NOT EXISTS outputs_fts_update
    AFTER UPDATE ON outputs BEGIN
    INSERT INTO outputs_fts(outputs_fts, rowid, content, output_type)
    VALUES ('delete', old.rowid, COALESCE(old.content,''), COALESCE(old.output_type,''));
    INSERT INTO outputs_fts(rowid, content, output_type)
    VALUES (new.rowid, COALESCE(new.content,''), COALESCE(new.output_type,''));
END;

CREATE TRIGGER IF NOT EXISTS outputs_fts_delete
    AFTER DELETE ON outputs BEGIN
    INSERT INTO outputs_fts(outputs_fts, rowid, content, output_type)
    VALUES ('delete', old.rowid, COALESCE(old.content,''), COALESCE(old.output_type,''));
END;

INSERT INTO outputs_fts(outputs_fts) VALUES('rebuild');

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (7, datetime('now'),
    'items.id=559 (decisions.id=421/422): outputs.status rebuilt to four-state document lifecycle (draft/finalized/potentially-stale/archived); soft-delete moved to new deleted_at column, independent of status; added document_relationship/parent_output_id/superseded_by for the document relationship model');
