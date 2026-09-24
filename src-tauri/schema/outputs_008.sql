-- outputs_008.sql
--
-- items.id=556 (decisions.id=826, decisions.id=421's exported_at gap-close):
-- adds outputs.exported_at, the timestamp decisions.id=421's
-- finalized -> potentially-stale transition needed but never got when
-- items.id=559 (outputs_007.sql) built the four-state status lifecycle.
--
-- Set when the new Library Export action fires the finalized ->
-- potentially-stale transition (output_store::export_output). Cleared on
-- return/re-import back to finalized (output_store::
-- return_output_from_export). Application-layer transition logic only --
-- this migration is schema/column only, same layering outputs_007.sql used
-- for status/document_relationship/parent_output_id/superseded_by.
--
-- Nullable, no default, no CHECK -- plain additive column, no table rebuild
-- needed. Unlike outputs_007.sql (which changed a CHECK constraint on
-- `status` and therefore required SQLite's rebuild-and-rename dance), this
-- column carries no constraint, so a straight ALTER TABLE ADD COLUMN
-- suffices, matching outputs_004.sql's precedent for nullable additive
-- columns (source/project_entity_id/focus_slug/storage_path/
-- storage_version/original_filename).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE outputs ADD COLUMN exported_at TEXT;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (8, datetime('now'),
    'items.id=556 (decisions.id=826/421): outputs.exported_at -- set on the Export-triggered finalized->potentially-stale transition, cleared on return/re-import back to finalized');
