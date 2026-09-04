-- outputs_005.sql
--
-- items.id=406 (decisions.id=756/757) -- Privacy Guardian persistence
-- cascade, outputs.db half. consent_decisions was append-only with no
-- per-fact granularity (span_id is an ephemeral per-invocation UUID, useless
-- as a lookup key across separate gate3 calls -- items.id=409 finding).
-- These columns make a prior decision queryable at gate-firing time instead
-- of purely a write-only audit row.
--
-- category/fact_key/original_text are nullable: only element_consent rows
-- populate them (gate3/floor rows have no single span they describe). A
-- populated fact_key is either a Layer 1 content hash, a Layer 2 within-
-- conversation coreference key, or a Layer 3 entity_facts-derived key
-- (conductor/privacy/fact_identity.rs) -- never a fuzzy/similarity match
-- (explicitly out of scope, decisions.id=757).
--
-- pf_fact_mentions is new: it is what stops gate3 being stateless
-- call-to-call within one conversation (items.id=406's own explicit flag).
-- It records every span mention seen so far in a focus_run_id regardless of
-- whether a decision has been made yet, so Layer 2 coreference has prior
-- mentions to resolve against even before the user has acted on any of them.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE consent_decisions ADD COLUMN category TEXT;
ALTER TABLE consent_decisions ADD COLUMN fact_key TEXT;
ALTER TABLE consent_decisions ADD COLUMN original_text TEXT;

CREATE INDEX IF NOT EXISTS idx_consent_decisions_fact_lookup
    ON consent_decisions (focus_run_id, fact_key)
    WHERE fact_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS pf_fact_mentions (
    id            TEXT NOT NULL PRIMARY KEY,
    focus_run_id  TEXT NOT NULL REFERENCES focus_runs(id),
    category      TEXT NOT NULL,
    fact_key      TEXT NOT NULL,
    original_text TEXT NOT NULL,
    created_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pf_fact_mentions_run
    ON pf_fact_mentions (focus_run_id, category);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (5, datetime('now'),
    'items.id=406 (decisions.id=756/757): consent_decisions.category/fact_key/original_text for prior-decision lookup at gate-firing time; new pf_fact_mentions table for within-conversation coreference state');
