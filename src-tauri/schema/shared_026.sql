-- shared_026.sql
--
-- items.id=708 (decisions.id=875 points 2 and 3, PROVIDER_REGISTRY_AND_TIER_
-- MODEL_SPEC.md Part 8c), absorbing items.id=703: schema and data for the
-- generic hosted-provider adapter. No behavior change here -- nothing reads
-- the new columns until items.id=709-713 and 724 land.
--
-- WHY A NEW MIGRATION, NOT A DIRECT EDIT (decisions.id=675 precedent does
-- not apply): providers, provider_models and user_provider_preference are
-- created in shared_013.sql and shared_016.sql (v13 and v16, run once per
-- database), and shared_016.sql seeds the model rows with a bare INSERT. An
-- edit there reaches no existing shared.db, and a v1 file may not hold
-- ALTER TABLE or bare DML (migrations.rs validate_v1_file_rerun_safety).
-- The schema consolidation cutover (items.id=633) folds this file into the
-- shared baseline.
--
-- providers: curator-owned columns, release-time seeding only, never
-- computed from other columns. api_dialect and auth_scheme are deliberately
-- open TEXT with no CHECK: wire formats are code and are added when a
-- curated row needs one (Part 8b), and SQLite cannot alter a CHECK without a
-- table rebuild. The application layer (items.id=709) validates them.
-- retention_days NULL means the retention period is unstated.
-- verification_status is a closed set, so it has a CHECK.
--
-- provider_models: output_overhead is a multiplier on the estimated output
-- tokens (reasoning tokens), 1.0 = none. pricing is JSON with its own
-- as_of, source_url and source (provider_api, curated or user).
-- context_window_source and context_window_as_of carry the same provenance
-- for context_window_tokens (spec 8f). The three *_override columns are
-- nullable per-model overrides of the provider's privacy attributes
-- (decisions.id=878 point 5); NULL means inherit from the provider row.
-- qr_recommended and qr_recommendation_rank on a model are a rank within
-- its provider, distinct from providers.qr_recommendation_rank.
--
-- user_provider_preference: plan_key (the user's plan at that provider,
-- matched against providers.plan_inclusion) and model_id (the selected
-- model, resolved Focus then Persona then account). model_id has no foreign
-- key: uncurated models from a live list are allowed (decisions.id=875
-- point 5). subscription_status is kept as legacy. This table is plaintext
-- in shared.db and reveals which providers a user uses -- items.id=661 will
-- move the whole table to the encrypted store.
--
-- DATA (items.id=703). The seeded default models were wrong: Groq retired
-- llama-3.1-8b-instant for free and developer tiers on 2026-08-16
-- (console.groq.com/docs/deprecations), and mistral-small-latest is an alias
-- that can re-point silently. The two seed rows are deleted (nothing
-- references provider_models.id, and the id is an opaque string,
-- decisions.id=813) and replaced by production defaults. The DELETE runs
-- before the INSERT because of the unique is_default partial index.
--
-- Values read on the build date 2026-10-11:
--   Groq openai/gpt-oss-20b: console.groq.com/docs/model/openai/gpt-oss-20b
--     lists context 131,072 tokens, max completion 65,536 tokens, input
--     0.075 and output 0.30 USD per million tokens. The 65,536 maximum has
--     no column here and is not a request default, so it is not stored.
--     Free-tier limits 30 RPM, 1K RPD, 8K TPM, 200K TPD match
--     console.groq.com/docs/rate-limits (that page labels its table as the
--     Developer plan base limits) and decisions.id=878 point 7, which read
--     the account limits page.
--   Mistral mistral-small-2603: docs.mistral.ai/models/mistral-small-4-0-26-03
--     lists context 256k and input 0.15 and output 0.60 USD per million
--     tokens. The page gives no exact token count, so 256000 is stored as
--     the conservative figure. Confirm max_context_length from the models
--     API with a key when items.id=724 finalizes the rows.
-- items.id=724 owns the final values for all providers. Mistral free-tier
-- limits, key page URL and retention are unverified and left empty.
--
-- Ranks and flags per decisions.id=878: Groq rank 1, Mistral rank 5, and
-- providers.qr_recommended is cleared for Mistral (a WARN posture is never
-- the provider-level recommendation). The hosted recommendation set is now
-- Groq only until items.id=724 enters the other clean providers.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE providers ADD COLUMN api_dialect TEXT;
ALTER TABLE providers ADD COLUMN base_url_template TEXT;
ALTER TABLE providers ADD COLUMN auth_scheme TEXT;
ALTER TABLE providers ADD COLUMN extra_headers TEXT
    CHECK (extra_headers IS NULL OR json_valid(extra_headers));
ALTER TABLE providers ADD COLUMN models_path TEXT;
ALTER TABLE providers ADD COLUMN user_variables TEXT
    CHECK (user_variables IS NULL OR json_valid(user_variables));
ALTER TABLE providers ADD COLUMN free_tier TEXT
    CHECK (free_tier IS NULL OR json_valid(free_tier));
ALTER TABLE providers ADD COLUMN plan_inclusion TEXT
    CHECK (plan_inclusion IS NULL OR json_valid(plan_inclusion));
ALTER TABLE providers ADD COLUMN retention_days INTEGER
    CHECK (retention_days IS NULL OR retention_days >= 0);
ALTER TABLE providers ADD COLUMN jurisdiction TEXT;
ALTER TABLE providers ADD COLUMN verification_status TEXT NOT NULL DEFAULT 'unverified'
    CHECK (verification_status IN ('verified', 'unverified', 'stale'));
ALTER TABLE providers ADD COLUMN credential_acquisition TEXT
    CHECK (credential_acquisition IS NULL OR json_valid(credential_acquisition));
ALTER TABLE providers ADD COLUMN spend_limit_url TEXT;
ALTER TABLE providers ADD COLUMN logo_key TEXT;

ALTER TABLE provider_models ADD COLUMN pricing TEXT
    CHECK (pricing IS NULL OR json_valid(pricing));
ALTER TABLE provider_models ADD COLUMN request_options TEXT
    CHECK (request_options IS NULL OR json_valid(request_options));
ALTER TABLE provider_models ADD COLUMN token_factor REAL NOT NULL DEFAULT 1.0
    CHECK (token_factor > 0);
ALTER TABLE provider_models ADD COLUMN output_overhead REAL NOT NULL DEFAULT 1.0
    CHECK (output_overhead >= 1.0);
ALTER TABLE provider_models ADD COLUMN status TEXT NOT NULL DEFAULT 'active'
    CHECK (status IN ('active', 'deprecated', 'retired'));
ALTER TABLE provider_models ADD COLUMN retires_at TEXT;
ALTER TABLE provider_models ADD COLUMN requires_paid INTEGER NOT NULL DEFAULT 0
    CHECK (requires_paid IN (0, 1));
ALTER TABLE provider_models ADD COLUMN qr_recommended INTEGER NOT NULL DEFAULT 0
    CHECK (qr_recommended IN (0, 1));
ALTER TABLE provider_models ADD COLUMN qr_recommendation_rank INTEGER
    CHECK (qr_recommendation_rank IS NULL OR qr_recommendation_rank >= 0);
ALTER TABLE provider_models ADD COLUMN retains_data_override INTEGER
    CHECK (retains_data_override IS NULL OR retains_data_override IN (0, 1));
ALTER TABLE provider_models ADD COLUMN trains_on_data_override INTEGER
    CHECK (trains_on_data_override IS NULL OR trains_on_data_override IN (0, 1));
ALTER TABLE provider_models ADD COLUMN retention_days_override INTEGER
    CHECK (retention_days_override IS NULL OR retention_days_override >= 0);
ALTER TABLE provider_models ADD COLUMN context_window_source TEXT
    CHECK (context_window_source IS NULL
        OR context_window_source IN ('provider_api', 'curated', 'user'));
ALTER TABLE provider_models ADD COLUMN context_window_as_of TEXT;

ALTER TABLE user_provider_preference ADD COLUMN plan_key TEXT;
ALTER TABLE user_provider_preference ADD COLUMN model_id TEXT;

-- Provider rows. Hosted rows only, matched by id because these are the two
-- rows shared_014.sql seeded. Everything else about them is untouched.
UPDATE providers SET
    api_dialect = 'openai_chat',
    base_url_template = 'https://api.groq.com/openai/v1',
    auth_scheme = 'bearer',
    extra_headers = '{}',
    models_path = '/models',
    user_variables = '[]',
    free_tier = '{"available":true,"requires_card":null,"limits":{"rpm":30,"rpd":1000,"tpm":8000,"tpd":200000},"applies_to":"openai/gpt-oss-20b","as_of":"2026-10-11","source_url":"https://console.groq.com/docs/rate-limits"}',
    plan_inclusion = '[]',
    retention_days = NULL,
    jurisdiction = 'United States',
    verification_status = 'verified',
    credential_acquisition = '{"method":"guided_paste","auth_url":null,"key_pattern":null,"keys_page_url":"https://console.groq.com/keys"}',
    spend_limit_url = 'https://console.groq.com/docs/spend-limits',
    qr_recommendation_rank = 1
WHERE id = 'groq';

UPDATE providers SET
    api_dialect = 'openai_chat',
    base_url_template = 'https://api.mistral.ai/v1',
    auth_scheme = 'bearer',
    extra_headers = '{}',
    models_path = '/models',
    user_variables = '[]',
    free_tier = '{"available":true,"requires_card":null,"limits":null,"applies_to":null,"as_of":"2026-10-11","source_url":null}',
    plan_inclusion = '[]',
    retention_days = NULL,
    jurisdiction = 'France (EU)',
    verification_status = 'unverified',
    credential_acquisition = '{"method":"guided_paste","auth_url":null,"key_pattern":null,"keys_page_url":null}',
    spend_limit_url = NULL,
    qr_recommended = 0,
    qr_recommendation_rank = 5
WHERE id = 'mistral';

-- Default models (items.id=703). Old seed rows out, production defaults in.
DELETE FROM provider_models
 WHERE id IN ('groq:llama-3.1-8b-instant', 'mistral:mistral-small-latest');

INSERT INTO provider_models
    (id, provider_id, model_id, context_window_tokens, is_default, created_at,
     pricing, request_options, status, qr_recommended, qr_recommendation_rank,
     context_window_source, context_window_as_of)
VALUES
    ('groq:openai/gpt-oss-20b', 'groq', 'openai/gpt-oss-20b', 131072, 1, datetime('now'),
     '{"input_per_mtok":0.075,"output_per_mtok":0.30,"currency":"USD","cache":null,"tiers":null,"as_of":"2026-10-11","source_url":"https://console.groq.com/docs/model/openai/gpt-oss-20b","source":"curated"}',
     '{}', 'active', 1, 1, 'curated', '2026-10-11'),
    ('mistral:mistral-small-2603', 'mistral', 'mistral-small-2603', 256000, 1, datetime('now'),
     '{"input_per_mtok":0.15,"output_per_mtok":0.60,"currency":"USD","cache":null,"tiers":null,"as_of":"2026-10-11","source_url":"https://docs.mistral.ai/models/mistral-small-4-0-26-03","source":"curated"}',
     '{}', 'active', 1, 1, 'curated', '2026-10-11');

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (26, datetime('now'),
    'items.id=708 (absorbs items.id=703): providers gains the decisions.id=875 adapter, disclosure and credential fields, provider_models gains pricing/status/privacy-override/context-window-provenance fields, user_provider_preference gains plan_key and model_id. groq and mistral rows populated as data (dialect, base URL, auth, free tier, jurisdiction, verification), old seed models replaced by openai/gpt-oss-20b and the pinned mistral-small-2603 with prices read 2026-10-11, Mistral qr_recommended cleared and ranks set per decisions.id=878');
