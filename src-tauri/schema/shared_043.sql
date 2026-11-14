-- shared_043.sql
--
-- items.id=894 part 1 (decisions.id=941 point 2, decisions.id=942): the mirror
-- rows for the three MODEL blocks. Data only: no table or column is added, so
-- the schema-shape golden does not change. Number checked against the live schema
-- directory on the build date: shared_042 was the latest. (decisions.id=942's
-- build table reserved shared_043 for the resolver and launch-question builds:
-- those take later numbers. Numbers before the items.id=633 cutover are
-- provisional, since the cutover consolidates every prefix to one baseline.)
--
-- building_blocks is the validation-only mirror of qr_docs.db's catalog
-- (shared_021.sql): a composition row may cite a stable_id only if it is a
-- member here as an inline_composable block, and then only if the registry
-- (conductor::block_dispatch::registered_block) has it. The three rows:
--   cb-12  Draft document          (model block, expands to a Generate step)
--   cb-13  Voice (apply or refine) (model block)
--   cb-14  Revise document         (model block)
-- A model block has NO BlockHandler: at LOAD it expands into an ordinary step
-- (conductor::model_block). The mirror goes from 10 rows to 13. The catalog rows
-- in qr_docs.db are written by Chat-PM from the items.id=894 handoff, and the
-- checked-in export under src-tauri/tests/fixtures/building_blocks_catalog.json
-- carries the proposed text until then.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

INSERT OR IGNORE INTO building_blocks
    (stable_id, display_name, invocation_mode, created_at, updated_at)
VALUES
    ('cb-12', 'Draft document', 'inline_composable', datetime('now'), datetime('now')),
    ('cb-13', 'Voice (apply or refine)', 'inline_composable', datetime('now'), datetime('now')),
    ('cb-14', 'Revise document', 'inline_composable', datetime('now'), datetime('now'));

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (43, datetime('now'),
    'items.id=894: building_blocks mirror rows for the three model blocks (cb-12 draft document, cb-13 voice, cb-14 revise) -- data only, no schema change');
