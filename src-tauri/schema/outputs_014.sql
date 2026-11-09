-- outputs_014.sql
--
-- items.id=881 follow-up (found during items.id=886): four columns that the
-- design of items.id=880 (handoff chat_session_handoffs.id=625) gave the stores
-- and outputs_013.sql, built by items.id=881, omitted. outputs_013.sql is not
-- edited: it is pushed, so the columns arrive as one migration.
--   1. case_records.focus_id: a case belongs to the Focus that opened it, and a
--      Focus sees the open cases of its own kind, not another Focus's.
--   2. config_deltas.focus_id: configuration history is never used in another
--      Focus without authorization (decisions.id=509), so a delta belongs to the
--      Focus that recorded it and the reads that feed a prompt filter on it.
--   3. config_deltas.source_case_id: the case a change was made for. It is a
--      real foreign key that is cleared, not cascaded, when the case is deleted
--      (a delta describes a device and outlives the ticket). session_reference
--      stays: it is the opaque id of the session the change was made in, the
--      provenance that decisions.id=508 and 510 surface as "we changed the DNS
--      settings in March". The two answer different questions.
--   4. case_strategies.rung_at: the escalation rung (1 to 5) the strategy was
--      tried at, NULL when the writer did not say.
--
-- case_records.focus_id is the Focus that opened the case (the running Focus's id, the same
-- value session_variables.focus_id carries). The store writes it at open_case
-- and a case keeps it for life. The file is the persona, so no persona_id column
-- is added (ruling (g) of decisions.id=939's addendum).
--
-- SAFE ON A POPULATED DATABASE: SQLite refuses ADD COLUMN NOT NULL without a
-- default, so the column is NOT NULL with an empty-text default. Nothing wrote a case
-- before this migration, so no row exists in practice. A case that somehow did
-- exist carries the empty focus_id, which no running Focus id equals: it is
-- listed for no Focus rather than for the wrong one. The store validates every
-- new focus_id as 1 to 128 characters, so the empty value never comes from a
-- writer.
--
-- config_deltas.focus_id follows the same rule: NOT NULL with an empty default,
-- validated 1 to 128 characters by the store, no backfill (nothing wrote a delta
-- before this migration). A delta with the empty value is listed for no Focus.
--
-- SQLite takes a REFERENCES column through ADD COLUMN only when its default is
-- NULL, which source_case_id has. The CHECK on rung_at is also allowed because
-- NULL satisfies it for every existing row.
--
-- The partial index on case_records is for the per-Focus open list: open cases
-- (closed_at IS NULL is exactly the non-terminal states, by the CHECK on
-- case_records) by Focus, most recently updated first. The config_deltas index
-- serves the per-Focus delta read, and the source_case_id index keeps the
-- ON DELETE SET NULL of a case purge from scanning the table.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals
-- and no comment text on a line that ends with a semicolon.
-- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE case_records ADD COLUMN focus_id TEXT NOT NULL DEFAULT '';

ALTER TABLE config_deltas ADD COLUMN focus_id TEXT NOT NULL DEFAULT '';

ALTER TABLE config_deltas ADD COLUMN source_case_id TEXT
    REFERENCES case_records(case_id) ON DELETE SET NULL;

ALTER TABLE case_strategies ADD COLUMN rung_at INTEGER
    CHECK (rung_at IS NULL OR rung_at BETWEEN 1 AND 5);

CREATE INDEX IF NOT EXISTS idx_case_records_open_by_focus
    ON case_records(focus_id, updated_at DESC)
    WHERE closed_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_config_deltas_focus
    ON config_deltas(focus_id, changed_at DESC);

CREATE INDEX IF NOT EXISTS idx_config_deltas_source_case
    ON config_deltas(source_case_id)
    WHERE source_case_id IS NOT NULL;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (14, datetime('now'),
    'items.id=881 follow-up: case_records.focus_id, config_deltas.focus_id and source_case_id, case_strategies.rung_at (handoff 625 design, decisions.id=509)');
