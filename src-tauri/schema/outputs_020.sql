-- outputs_020.sql
--
-- items.id=903 unit B (decisions.id=942 Q14 to Q16, decisions.id=944, decisions.id=963):
-- the DOCUMENT SESSION of a Writing Assistant chat, and the document privacy flag
-- carried onto a saved Library document. Schema only. The typed store is
-- persistence/document_session_store.rs.
--
-- WHAT A DOCUMENT SESSION IS. Multi-turn writing rides on chat-level passes
-- (decisions.id=938 ruling 2): each user message is a new run of the Focus in the same
-- chat. What those passes share about the DOCUMENT itself, as opposed to its text,
-- lives in one row per (context_key, focus_id):
--   document_type          the document type the row was written for (a shipped id,
--                          or later a custom one), so a raise can be voided when the
--                          user changes the type;
--   document_privacy_flag  NULL or one of general, personal, medical, financial,
--                          legal: the user's own label for this document, chosen after
--                          the output scan. medical, financial and legal make every
--                          later pass of the document local-only (the run's
--                          max_permitted ceiling becomes local_only), unless the
--                          one-document ceiling raise of items.id=903 unit D applies.
--                          A closed Rust enum (DocumentPrivacyFlag), NOT a SQL CHECK,
--                          for the reason consent_audit_records gave (personal_010):
--                          a later value would be a table rebuild after first ship;
--   raised_ceiling         NULL, or the tier of a one-document ceiling raise (unit D,
--                          Path B of decisions.id=942 Q14). Written ONLY by unit D's
--                          confirm command, after its audit row;
--   consent_audit_id       NULL, or the consent_audit_records id (personal.db,
--                          decisions.id=944) of the event that raised it.
-- NO CONTENT COLUMN. The document's text is in pass_artifacts (working_draft) and,
-- once saved, in outputs. Nothing here echoes what the user wrote.
--
-- outputs.document_privacy_flag: the same flag carried onto a saved document, so the
-- Library knows what the user called it. Nullable, no default, no CHECK (the same
-- shape as outputs_018 and outputs_019). outputs.sensitivity is left alone: its CHECK
-- has no 'legal', and it is the scan's label, not the user's.
--
-- RETENTION AND PURGE. Keyed by context_key like session_variables and
-- pass_artifacts, so the archive hook (persistence::chat_clear) deletes a chat's rows
-- and the 30-day store sweep (persistence::store_sweep) deletes rows not written for
-- STORE_RETENTION_DAYS, on idx_document_sessions_updated_at.
--
-- NO FOREIGN KEYS, NO TRIGGERS, NO DROP TABLE. items.id=633 must absorb this file with
-- the other outputs files.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS document_sessions (
    context_key            TEXT NOT NULL CHECK (length(context_key) BETWEEN 1 AND 128),
    focus_id               TEXT NOT NULL CHECK (length(focus_id) BETWEEN 1 AND 128),
    document_type          TEXT NOT NULL CHECK (length(document_type) BETWEEN 1 AND 64),
    document_privacy_flag  TEXT,
    raised_ceiling         TEXT,
    consent_audit_id       TEXT,
    version                INTEGER NOT NULL DEFAULT 1 CHECK (version >= 1),
    created_at             TEXT NOT NULL,
    updated_at             TEXT NOT NULL,
    PRIMARY KEY (context_key, focus_id)
);

CREATE INDEX IF NOT EXISTS idx_document_sessions_updated_at
    ON document_sessions(updated_at);

ALTER TABLE outputs ADD COLUMN document_privacy_flag TEXT;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (20, datetime('now'),
    'items.id=903: document_sessions (document type, privacy flag, one-document raise) and outputs.document_privacy_flag');
