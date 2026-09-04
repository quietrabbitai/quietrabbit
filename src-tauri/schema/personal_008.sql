-- personal_008.sql
--
-- items.id=406 (decisions.id=756/757) -- Privacy Guardian persistence
-- cascade, personal.db half. disclosure_log needed no new retention (it was
-- already append-only/durable, D6-198) but had no per-fact query path
-- (single index was (focus_run_id, created_at) only -- items.id=409
-- finding). category/fact_key make a specific fact's disclosure history
-- queryable at gate-firing time, same identity scheme as outputs_005.sql's
-- consent_decisions columns.
--
-- pf_standing_preferences is the Persona-scoped opt-in standing preference
-- (decisions.id=756: "remember this for [Persona]", off by default, reusing
-- the D5-152 save_preference:bool trigger pattern). It is DELIBERATELY its
-- own table here in personal.db (SQLCipher-encrypted), NOT a reuse of
-- personas.extra_metadata (shared.db, UNENCRYPTED) the way
-- write_floor_consent_preference stores floor consent. Floor consent's
-- saved value is a bare abstraction_tier integer -- not privacy-sensitive on
-- its own. A Privacy Guardian standing preference is fact-linked and can be
-- pinned to a specific email/phone/person; storing that in unencrypted
-- shared.db would undermine the entire feature. This is a deliberate
-- deviation from a literal reading of "reuse write_floor_consent_preference"
-- -- the D5-152 *pattern* (explicit opt-in boolean, separate write) is
-- reused; the *storage location* is not.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE disclosure_log ADD COLUMN category TEXT;
ALTER TABLE disclosure_log ADD COLUMN fact_key TEXT;

CREATE INDEX IF NOT EXISTS idx_disclosure_log_fact_lookup
    ON disclosure_log (persona_id, fact_key)
    WHERE fact_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS pf_standing_preferences (
    id                  TEXT NOT NULL PRIMARY KEY,
    persona_id          TEXT NOT NULL,
    fact_key            TEXT NOT NULL,
    category            TEXT NOT NULL,
    decision            TEXT NOT NULL
                            CHECK (decision IN ('generalize', 'keep_private', 'release_original')),
    suggestion_text     TEXT,
    user_modified_text  TEXT,
    created_at          TEXT NOT NULL,
    UNIQUE (persona_id, fact_key)
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (8, datetime('now'),
    'items.id=406 (decisions.id=756/757): disclosure_log.category/fact_key for per-fact query at gate-firing time; new pf_standing_preferences table -- encrypted, Persona-scoped opt-in standing decisions, deliberately not personas.extra_metadata');
