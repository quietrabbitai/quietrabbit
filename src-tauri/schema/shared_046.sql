-- shared_046.sql
--
-- items.id=387 stage 3 (decisions.id=942 point 7): the mirror row for the RENDER
-- block cb-15, the manual hand-off starter. Data only: no table or column is
-- added, so the schema-shape golden does not change. Number checked against the
-- live schema directory on the build date: shared_045 was the latest.
-- (items.id=633 must absorb this file with the others. Numbers before that
-- cutover are provisional, since the cutover consolidates every prefix to one
-- baseline.)
--
-- building_blocks is the validation-only mirror of qr_docs.db's catalog
-- (shared_021.sql): a composition row may cite a stable_id only if it is a member
-- here as an inline_composable block, and then only if the registry
-- (conductor::block_dispatch::registered_block) has it. cb-15 is the first block
-- of the fourth kind, render (conductor::handoff_render): it makes no model call
-- and has no BlockHandler. The mirror goes from 13 rows to 14.
--
-- The catalog row in qr_docs.db is Chat-PM's, and the checked-in export under
-- src-tauri/tests/fixtures/building_blocks_catalog.json carries its text.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

INSERT OR IGNORE INTO building_blocks
    (stable_id, display_name, invocation_mode, created_at, updated_at)
VALUES
    ('cb-15', 'Manual hand-off starter', 'inline_composable', datetime('now'), datetime('now'));

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (46, datetime('now'),
    'items.id=387: building_blocks mirror row for the render block cb-15 (manual hand-off starter) -- data only, no schema change');
