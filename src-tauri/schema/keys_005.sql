-- persistence/schema/keys_005.sql
-- Per-user integration keys database schema: integration_keys.db
-- Migration version: 5
--
-- items.id=711 (decisions.id=875 points 5 and 6, PROVIDER_REGISTRY_AND_TIER_
-- MODEL_SPEC.md Part 8e-8f): the per-user state for the live model list and
-- for calibration. A new numbered migration, not an edit of keys_003/004:
-- those have been applied to real databases (open_integration_keys_db always
-- runs the migrator).
--
-- WHY PER USER AND ENCRYPTED: a live model list depends on the user's own key
-- and plan, so "present in the last list" differs between users, and the rows
-- reveal which providers and models a user touches. provider_models in
-- shared.db is global, curator-owned and plaintext (decisions.id=862), so
-- nothing the user discovers is written there.
--
-- provider_live_models: one row per model id a refresh has ever returned for
-- a provider. present = 1 while it is in the latest list; a refresh that no
-- longer returns it sets present = 0 (a flag, never a delete: a model that
-- is missing is only MAY BE RETIRED, and a stored selection is never
-- silently dropped). context_tokens and retires_at are copied from the
-- provider's list where it reports them (NULL = not reported). The context
-- window source of these rows is always the provider's API, dated
-- context_as_of. user_output_limit is the output token limit the user chose
-- when selecting an uncurated model (the client fails closed without one).
--
-- provider_model_refresh: one row per provider, written by each refresh. It
-- is how a reader tells "never refreshed" (nothing is known to be missing)
-- from "refreshed, and a curated model is absent from the list".
--
-- calibration.output_sample_count: calibration.sample_count (keys_003) counts
-- input samples. Output (hidden reasoning) samples only exist when the
-- provider reports reasoning tokens, so they have their own count.
--
-- No cross-database foreign keys (provider ids live in shared.db).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS provider_live_models (
    provider_id         TEXT NOT NULL,
    model_id            TEXT NOT NULL,
    context_tokens      INTEGER CHECK (context_tokens IS NULL OR context_tokens > 0),
    context_as_of       TEXT,
    retires_at          TEXT,
    first_seen_at       TEXT NOT NULL,
    last_seen_at        TEXT NOT NULL,
    present             INTEGER NOT NULL DEFAULT 1 CHECK (present IN (0, 1)),
    user_output_limit   INTEGER CHECK (user_output_limit IS NULL OR user_output_limit > 0),
    PRIMARY KEY (provider_id, model_id)
);

CREATE TABLE IF NOT EXISTS provider_model_refresh (
    provider_id         TEXT PRIMARY KEY,
    refreshed_at        TEXT NOT NULL,
    listed_count        INTEGER NOT NULL CHECK (listed_count >= 0)
);

ALTER TABLE calibration ADD COLUMN output_sample_count INTEGER NOT NULL DEFAULT 0
    CHECK (output_sample_count >= 0);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (5, datetime('now'), 'items.id=711: provider_live_models, provider_model_refresh and calibration.output_sample_count for the per-user live model list and calibration (decisions.id=875)');
