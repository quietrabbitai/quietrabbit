-- persistence/schema/keys_003.sql
-- Per-user integration keys database schema: integration_keys.db
-- Migration version: 3
--
-- items.id=708 (decisions.id=875 point 3, PROVIDER_REGISTRY_AND_TIER_MODEL_
-- SPEC.md Part 8c-8e): the per-user encrypted tables for hosted-provider
-- consent, limits, spend ledger, price overrides and calibration. They live
-- here, not in shared.db, because shared.db is plaintext and these rows
-- reveal which providers a user uses and how much they spend (decisions.id=
-- 862). Tables only: no code reads or writes them until items.id=710 and
-- 711.
--
-- A new numbered migration, not an edit of keys_001.sql: keys_001 is a v1
-- file (re-run on every migrate) and has been applied to real databases.
-- integration_keys_store::open_integration_keys_db now always runs the
-- migrator, so existing databases pick this file up (items.id=389).
--
-- No cross-database foreign keys, same reason as keys_001.sql's persona_id:
-- provider and model ids live in shared.db, a separate SQLite file. All ids
-- here are plain TEXT, validated by the application layer.
--
-- Money is stored as integer micro-USD (1 USD = 1,000,000), never REAL, so
-- ledger sums do not drift. Prices (price_overrides.pricing) are JSON of the
-- same shape as provider_models.pricing, with source "user".
--
-- provider_consent is an append-only log (explicit, logged consent is a
-- fixed protection, decisions.id=872). The current consent for a provider is
-- its latest row with revoked_at NULL. attributes_hash is a hash of the
-- disclosed attributes shown at consent, so consent is asked again when a
-- verified attribute changes (Part 8d).
--
-- provider_limits: limit_value is micro-USD for the cost_usd metric and a
-- plain count for the others. scope_id is NULL exactly when scope_kind is
-- all. The unique index folds NULL scope_id to an empty string because
-- SQLite treats NULLs as distinct in a plain UNIQUE.
--
-- provider_usage_ledger: cost_micros NULL means the price was unknown when
-- the call was recorded (uncurated model, no override).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS provider_consent (
    id                  TEXT PRIMARY KEY,
    provider_id         TEXT NOT NULL,
    attributes_hash     TEXT NOT NULL,
    consented_at        TEXT NOT NULL,
    revoked_at          TEXT
);

CREATE INDEX IF NOT EXISTS idx_provider_consent_provider
    ON provider_consent (provider_id, consented_at);

CREATE TABLE IF NOT EXISTS provider_limits (
    id                  TEXT PRIMARY KEY,
    metric              TEXT NOT NULL
                            CHECK (metric IN ('cost_usd', 'tokens_in', 'tokens_out', 'requests')),
    period              TEXT NOT NULL
                            CHECK (period IN ('run', 'day', 'week', 'month')),
    scope_kind          TEXT NOT NULL
                            CHECK (scope_kind IN ('all', 'provider', 'persona', 'focus')),
    scope_id            TEXT,
    limit_value         INTEGER NOT NULL CHECK (limit_value > 0),
    warn_at_pct         INTEGER NOT NULL DEFAULT 80
                            CHECK (warn_at_pct BETWEEN 1 AND 100),
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    CHECK ((scope_kind = 'all') = (scope_id IS NULL))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_provider_limits_scope
    ON provider_limits (metric, period, scope_kind, COALESCE(scope_id, ''));

CREATE TABLE IF NOT EXISTS provider_usage_ledger (
    id                  TEXT PRIMARY KEY,
    recorded_at         TEXT NOT NULL,
    provider_id         TEXT NOT NULL,
    model_id            TEXT NOT NULL,
    persona_id          TEXT,
    focus_id            TEXT,
    run_id              TEXT,
    tokens_in           INTEGER NOT NULL DEFAULT 0 CHECK (tokens_in >= 0),
    tokens_out          INTEGER NOT NULL DEFAULT 0 CHECK (tokens_out >= 0),
    requests            INTEGER NOT NULL DEFAULT 1 CHECK (requests >= 0),
    cost_micros         INTEGER CHECK (cost_micros IS NULL OR cost_micros >= 0),
    price_source        TEXT NOT NULL DEFAULT 'unknown'
                            CHECK (price_source IN ('provider_api', 'curated', 'user', 'unknown'))
);

CREATE INDEX IF NOT EXISTS idx_provider_usage_ledger_time
    ON provider_usage_ledger (recorded_at);

CREATE INDEX IF NOT EXISTS idx_provider_usage_ledger_provider
    ON provider_usage_ledger (provider_id, recorded_at);

CREATE TABLE IF NOT EXISTS price_overrides (
    provider_id         TEXT NOT NULL,
    model_id            TEXT NOT NULL,
    pricing             TEXT NOT NULL CHECK (json_valid(pricing)),
    updated_at          TEXT NOT NULL,
    PRIMARY KEY (provider_id, model_id)
);

CREATE TABLE IF NOT EXISTS calibration (
    provider_id         TEXT NOT NULL,
    model_id            TEXT NOT NULL,
    input_factor        REAL NOT NULL DEFAULT 1.0 CHECK (input_factor > 0),
    output_factor       REAL NOT NULL DEFAULT 1.0 CHECK (output_factor > 0),
    sample_count        INTEGER NOT NULL DEFAULT 0 CHECK (sample_count >= 0),
    updated_at          TEXT NOT NULL,
    PRIMARY KEY (provider_id, model_id)
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (3, datetime('now'), 'items.id=708: provider_consent, provider_limits, provider_usage_ledger, price_overrides and calibration tables for hosted-provider consent, limits, spend and calibration (decisions.id=875)');
