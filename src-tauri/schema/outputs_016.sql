-- outputs_016.sql
--
-- items.id=884 (decisions.id=943 point 5, decisions.id=937 point 4): the record of
-- the OUTBOUND REVIEW of the rendered prompt of an external Focus step,
-- step_reviews, in the persona's outputs.db. Schema only. The typed store is
-- persistence/step_review_store.rs and the one module that decides anything is
-- conductor/step_review.rs. (items.id=895 took outputs_015.sql.)
--
-- ONE ROW PER REVIEW OF ONE PROMPT. A review starts when a step that goes to a
-- hosted provider has rendered its prompt and the Privacy Filter has looked at it.
-- The row is keyed by the run, the action and the SHA-256 of the rendered text
-- (prompt_sha), so a changed text is a different review, never the old answer.
-- A row is written PENDING when the user must answer (the step parks), and moves
-- to its final outcome when the answers are applied. A clean or standing-only
-- review is written once, already final.
--
-- OUTCOME is a CLOSED set held as a Rust enum (ReviewOutcome) and NOT as a SQL
-- CHECK, the choice pass_artifacts and consent_audit_records made, so a later
-- outcome is a code change plus a round-trip test and not a table rebuild:
--   final:    clean, standing_applied, approved_as_shown, edited, redacted
--   ended:    refused (the run was cancelled), blocked (the step parked)
--   internal: pending, superseded
-- edited wins over redacted. The counts keep the detail.
--
-- WHAT IS STORED. No prompt text. spans_json holds one descriptor per span the
-- filter flagged: id, byte offsets, category, review tier, whether the user was
-- asked or a standing decision settled it, and for a settled span the decision and
-- the replacement text that decision carries (a generalisation or the removal
-- marker, which is what leaves the device and never the original words). What the
-- user answered lives in consent_decisions by span id, as before. outbound_sha and
-- outbound_len are the hash and size of the text that was released. That is enough
-- to rebuild the outbound text on a retry or a resume (the text is re-rendered,
-- re-hashed, and the descriptors and answers are applied to it) and to prove that
-- what was sent is what was reviewed. reason is a short code (never a sentence and
-- never text) for blocked rows. provider_id is the provider of the selected model.
--
-- items.id=901 (run plan) and items.id=913 (paid API for Writing Assistant) read
-- this table by run and action.
--
-- FOREIGN KEY. focus_run_id REFERENCES focus_runs(id), as consent_decisions does.
-- items.id=633 rebuilds focus_runs and must carry this table with the other
-- children (migrations.rs FOCUS_RUN_TABLES). No triggers. No DROP TABLE.
--
-- RETENTION. A row holds hashes, offsets and categories. It is deleted with its
-- run only if a purge of runs is ever built (nothing deletes focus_runs today).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals
-- and no comment text on a line that ends with a semicolon.
-- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS step_reviews (
    review_id        TEXT PRIMARY KEY,
    focus_run_id     TEXT NOT NULL REFERENCES focus_runs(id),
    action_id        TEXT NOT NULL CHECK (length(action_id) BETWEEN 1 AND 256),
    prompt_sha       TEXT NOT NULL CHECK (length(prompt_sha) = 64),
    prompt_len       INTEGER NOT NULL CHECK (prompt_len >= 0),
    spans_json       TEXT NOT NULL DEFAULT '[]',
    total_spans      INTEGER NOT NULL DEFAULT 0 CHECK (total_spans >= 0),
    auto_resolved    INTEGER NOT NULL DEFAULT 0 CHECK (auto_resolved >= 0),
    released         INTEGER NOT NULL DEFAULT 0 CHECK (released >= 0),
    generalized      INTEGER NOT NULL DEFAULT 0 CHECK (generalized >= 0),
    removed          INTEGER NOT NULL DEFAULT 0 CHECK (removed >= 0),
    outcome          TEXT NOT NULL CHECK (length(outcome) BETWEEN 1 AND 32),
    reason           TEXT CHECK (reason IS NULL OR length(reason) BETWEEN 1 AND 64),
    outbound_sha     TEXT CHECK (outbound_sha IS NULL OR length(outbound_sha) = 64),
    outbound_len     INTEGER CHECK (outbound_len IS NULL OR outbound_len >= 0),
    provider_id      TEXT,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_step_reviews_run_action
    ON step_reviews(focus_run_id, action_id, created_at);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (16, datetime('now'),
    'items.id=884: outbound review record step_reviews (decisions.id=943 point 5, decisions.id=937 point 4)');
