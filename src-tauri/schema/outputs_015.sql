-- outputs_015.sql
--
-- items.id=895 (decisions.id=942 architecture point 5, decisions.id=938 ruling 2,
-- plan label B3a): the per-pass ARTIFACT STORE, pass_artifacts, in the persona's
-- outputs.db. Schema only. The typed store API is persistence/pass_artifact_store.rs
-- and the typed writers are the artifact_put and artifact_clear operations of
-- conductor::store_op.
--
-- WHY A SECOND TABLE BESIDE session_variables. Multi-turn work rides on chat-level
-- passes (decisions.id=938 ruling 2): each user message is a new run, and what a
-- later pass needs from an earlier one must be stored. A session variable holds at
-- most 2048 bytes (outputs_013). A content brief, voice guidance, a validation
-- report and a list of delta items are longer than that and must survive between
-- passes, so they get their own table with a larger cap and their own typed
-- writers. They are still the same kind of thing as a session variable: scoped to
-- one chat and one Focus, version compare-and-set, refused once the chat is a
-- one-time session.
--
-- ONE ROW PER (context_key, focus_id, kind). A write replaces the row's content
-- and moves version up by one. There is no revision history: the store keeps the
-- current artifact only, so no earlier text is retained. kind is a CLOSED set,
-- content_brief, voice_guidance, validation_report and delta_items, held as a Rust
-- enum (ArtifactKind) and NOT as a SQL CHECK. A new kind is a code change plus a
-- round-trip test, and a CHECK would make a later kind a table rebuild after first
-- ship (the choice consent_audit_records made, personal_010.sql).
--
-- content is capped at 16384 bytes here and in pass_artifact_store.rs and is never
-- empty. It is the artifact text, which can echo what the user wrote, so it stays
-- in the persona's encrypted outputs.db (decisions.id=862: not shared.db, not a
-- new file, not personal.db). The file is the persona, so no row carries a
-- persona_id. Logs carry counts and hashes only, never this text.
--
-- NO FOREIGN KEYS. written_by_run is a plain TEXT run id with no constraint,
-- because items.id=633 rebuilds focus_runs. No triggers. No DROP TABLE.
--
-- RETENTION AND PURGE. The table is keyed by context_key like session_variables,
-- so the chat-level clear and the retention sweep (both items.id=888) delete
-- by context_key and updated_at. Neither exists yet and this item does not
-- add them. artifact_clear (one kind of one chat and Focus) is the only delete.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals
-- and no comment text on a line that ends with a semicolon.
-- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS pass_artifacts (
    context_key     TEXT NOT NULL CHECK (length(context_key) BETWEEN 1 AND 128),
    focus_id        TEXT NOT NULL CHECK (length(focus_id) BETWEEN 1 AND 128),
    kind            TEXT NOT NULL CHECK (length(kind) BETWEEN 1 AND 64),
    content         TEXT NOT NULL CHECK (length(CAST(content AS BLOB)) BETWEEN 1 AND 16384),
    version         INTEGER NOT NULL DEFAULT 1 CHECK (version >= 1),
    written_by_run  TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (context_key, focus_id, kind)
);

CREATE INDEX IF NOT EXISTS idx_pass_artifacts_updated_at
    ON pass_artifacts(updated_at);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (15, datetime('now'),
    'items.id=895: per-pass artifact store pass_artifacts (decisions.id=942 point 5, decisions.id=938 ruling 2)');
