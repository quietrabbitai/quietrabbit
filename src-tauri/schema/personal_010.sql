-- personal_010.sql
--
-- items.id=920 stage 1 (decisions.id=944, decisions.id=509, decisions.id=434):
-- consent_audit_records, the permanent audit trail of consent events, in each
-- Persona's encrypted personal.db next to disclosure_log (personal_001.sql).
-- It is NOT in shared.db: shared.db may hold only what must be readable before
-- login (decisions.id=862), and a consent record shows which Focuses a Persona
-- uses and when. Number checked against the live schema directory on the build
-- date: personal_009 was the latest. Numbers before the items.id=633 cutover are
-- provisional, since the cutover folds every prefix into one baseline.
--
-- APPEND-ONLY, PERMANENT, like disclosure_log: the only module that touches the
-- table is persistence/consent_audit_store.rs, which has append, list and
-- latest_for_focus and no UPDATE or DELETE (a source-scan test pins it). A purge
-- is a separate, user-initiated function and is not part of this file.
--
-- COLUMNS
--   id                     the event id (a UUID).
--   event_type             one of the strings of the Rust enum
--                          consent_audit_store::ConsentEventType. No CHECK here:
--                          SQLite cannot alter a CHECK without a table rebuild,
--                          so the enum validates the value and a round-trip test
--                          pins it. Today: collection_consent_granted (a
--                          re-consent is the same type pointing at the prior
--                          event with a new text_version),
--                          collection_consent_withdrawn, document_ceiling_raised
--                          (carries document_type), and the five types of
--                          decisions.id=434 (tier3_generation, tier3_validation,
--                          sensitivity_override, standing_consent_use,
--                          output_privacy_disclosure).
--   focus_slug             the Focus the event belongs to.
--   run_id                 the run, NULL for a setup event.
--   text_version           the version of the consent text that was shown.
--   consent_reference_id   the prior event in the chain (a re-consent or a
--                          withdrawal names the grant it follows).
--   content_summary        a FIXED abstract description of the event, never user
--                          content.
--   document_type          set for document_ceiling_raised only.
--   created_at             when the event was recorded.
-- There is no persona_id column: the file boundary implies it, as in
-- persona_floor_consent (personal_009.sql).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS consent_audit_records (
    id                    TEXT PRIMARY KEY,
    event_type            TEXT NOT NULL,
    focus_slug            TEXT NOT NULL,
    run_id                TEXT,
    text_version          INTEGER,
    consent_reference_id  TEXT,
    content_summary       TEXT NOT NULL,
    document_type         TEXT,
    created_at            TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_consent_audit_records_focus
    ON consent_audit_records (focus_slug, created_at);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (10, datetime('now'),
    'items.id=920 (decisions.id=944): consent_audit_records, the append-only consent audit trail in the encrypted personal.db');
