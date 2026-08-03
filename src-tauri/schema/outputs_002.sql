-- outputs_002.sql
--
-- decisions.id=623 scoped a shared extraction/classification function with
-- three named callers (Historian batch, document-ingestion Pathway A, Quick
-- Close), each tagging its output with a `source` provenance value for the
-- eventual extract_confirm_candidates row. items.id=181's Chat-DEV
-- feasibility review (2026-08-02) confirmed the column was never actually
-- added to the live table. Gap tracked as items.id=211.
--
-- ONE NEW COLUMN on extract_confirm_candidates:
--   source  text enum, nullable, no default
--
-- ONLY THE THREE VALUES decisions.id=623 ITSELF NAMES: conductor_batch,
-- ingest_pathway_a, quick_close. items.id=181 separately found that My Facts
-- needs a 4th real-time/on-demand trigger context, but no decision or item
-- names that value yet — it belongs to items.id=176's still-active design
-- session. Adding an invented placeholder here would risk colliding with
-- whatever string that session lands on; widening the CHECK is a cheap
-- follow-up migration once it's named.
--
-- NULLABLE, NO DEFAULT: unlike this table's other enum columns (sensitivity,
-- status), source is being retrofitted onto a table that may already have
-- real rows, and no decision specifies a backfill value for them. SQLite's
-- CHECK only evaluates non-null values, so existing rows land as NULL
-- without needing an invented sentinel value.
--
-- SQLite CAN add columns with ALTER TABLE ADD COLUMN without a full rebuild
-- — this is a pure additive, nullable column; no existing row loses data.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals — parse_statements() is not a general-purpose SQL parser.

ALTER TABLE extract_confirm_candidates
    ADD COLUMN source TEXT
        CHECK (source IN ('conductor_batch', 'ingest_pathway_a', 'quick_close'));

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (2, datetime('now'),
    'decisions.id=623 source provenance column (items.id=211): extract_confirm_candidates.source');
