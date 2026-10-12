-- persistence/schema/keys_004.sql
-- Per-user integration keys database schema: integration_keys.db
-- Migration version: 4
--
-- items.id=710 (decisions.id=875 points 3 and 4, decisions.id=872 points 3
-- and 6, PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md Part 8d and 8e): the shape
-- the consent, limits and spend-ledger code needs on top of keys_003.sql.
-- A new numbered migration, not an edit of keys_003.sql: keys_003 has been
-- applied to real databases (integration_keys_store::open_integration_keys_db
-- always runs the migrator, so existing databases pick this file up).
--
-- provider_limits.approves_unattended: the user's explicit approval that an
-- unattended or background run may spend within this limit. Default 0, so no
-- existing or new limit authorizes anything until the user marks it. Only a
-- per-run cost limit can carry it (the CHECK below), because the approval is
-- a per-run budget (no-autonomous-action tenet, Part 8e).
--
-- provider_usage_ledger: reasoning_tokens records hidden reasoning tokens
-- when the provider reports them (they are inside tokens_out) so items.id=711
-- can calibrate output_overhead. outcome and usage_source say how the row was
-- settled. A row is first written as a reservation (outcome pending) holding
-- the worst-case amounts in the normal columns, so concurrent calls cannot
-- both slip under a cap. Settlement overwrites those columns with actual
-- usage. A call that reached the provider but returned no usage, and a
-- reservation left pending by a crash, end as outcome interrupted with
-- usage_source none and the reserved amounts kept (charged at the worst
-- case). reserved_micros keeps the reserved cost for audit after settlement.
--
-- provider_consent.no_cap_confirmed: the user explicitly confirmed consenting
-- to a metered provider with no spend cap (Part 8e: no cap only after an
-- explicit confirm).
--
-- provider_rate_snapshot: the latest rate-limit observation per provider,
-- taken from response headers (remaining free-tier quota, Part 8e). One row
-- per provider, replaced on each observation. raw holds the captured header
-- names and values as JSON, size-capped by the writer.
--
-- Money stays integer micro-USD. No cross-database foreign keys (provider ids
-- live in shared.db).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE provider_limits ADD COLUMN approves_unattended INTEGER NOT NULL DEFAULT 0
    CHECK (approves_unattended IN (0, 1)
           AND (approves_unattended = 0 OR (metric = 'cost_usd' AND period = 'run')));

ALTER TABLE provider_usage_ledger ADD COLUMN reasoning_tokens INTEGER
    CHECK (reasoning_tokens IS NULL OR reasoning_tokens >= 0);

ALTER TABLE provider_usage_ledger ADD COLUMN outcome TEXT NOT NULL DEFAULT 'ok'
    CHECK (outcome IN ('pending', 'ok', 'truncated', 'budget_exhausted', 'failed', 'interrupted'));

ALTER TABLE provider_usage_ledger ADD COLUMN usage_source TEXT NOT NULL DEFAULT 'reported'
    CHECK (usage_source IN ('reported', 'estimated', 'none'));

ALTER TABLE provider_usage_ledger ADD COLUMN reserved_micros INTEGER
    CHECK (reserved_micros IS NULL OR reserved_micros >= 0);

CREATE INDEX IF NOT EXISTS idx_provider_usage_ledger_run
    ON provider_usage_ledger (run_id);

ALTER TABLE provider_consent ADD COLUMN no_cap_confirmed INTEGER NOT NULL DEFAULT 0
    CHECK (no_cap_confirmed IN (0, 1));

CREATE TABLE IF NOT EXISTS provider_rate_snapshot (
    provider_id         TEXT PRIMARY KEY,
    observed_at         TEXT NOT NULL,
    requests_limit      INTEGER CHECK (requests_limit IS NULL OR requests_limit >= 0),
    requests_remaining  INTEGER CHECK (requests_remaining IS NULL OR requests_remaining >= 0),
    tokens_limit        INTEGER CHECK (tokens_limit IS NULL OR tokens_limit >= 0),
    tokens_remaining    INTEGER CHECK (tokens_remaining IS NULL OR tokens_remaining >= 0),
    requests_reset      TEXT,
    tokens_reset        TEXT,
    raw                 TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(raw))
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (4, datetime('now'), 'items.id=710: provider_limits.approves_unattended, ledger reasoning_tokens/outcome/usage_source/reserved_micros, provider_consent.no_cap_confirmed and the provider_rate_snapshot table for hosted-provider consent, limits and spend (decisions.id=875)');
