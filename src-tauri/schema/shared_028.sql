-- shared_028.sql
--
-- items.id=724 (decisions.id=878, 872, 875): the curated R1 hosted-provider
-- and model rows, as DATA. No schema change. Eight providers are added and
-- the Groq and Mistral rows from shared_026/027 are reconciled with the
-- FINAL curated records (Working/PROVIDER_DUE_DILIGENCE_20261011.md).
--
-- WHY A NEW MIGRATION: shared_026.sql and shared_027.sql are applied files
-- (c8b30c0, bea3231) and are never edited in place.
--
-- BUILD DATE 2026-10-11. Every price, limit, id and URL below was read on
-- that date from the provider documentation, pricing or model pages named in
-- the row. NO provider API host was contacted (no live call, no key, items.id
-- =724 scope). Only Groq has ever been exercised live (items.id=709). The
-- anthropic_messages dialect, the Cloudflare per-user URL and every other
-- provider are covered by loopback tests only.
--
-- IDS. claude, gemini and chatgpt are Cloud Chat rows already, so the API
-- rows are anthropic_api and gemini_api. Provider ids are opaque (decisions.
-- id=813).
--
-- POSTURE TO COLUMNS (decisions.id=878, mapping approved on items.id=724):
--   clean: trains_on_data_by_default 0, risk_rating 1, level low
--   conditional or warn: risk_rating 2, level medium
--   strong warn or unverified: risk_rating 3, level high
--   retains_data 1 when retention is stated or not stated, 0 when the provider
--   says nothing is retained. retention_days NULL means not stated.
--   verification_status verified means the attributes were checked against
--   primary pages on the build date. It does not mean the provider states
--   everything: gaps are disclosed in documentation_gate.
--   Only Groq, Ollama Cloud and Cloudflare are qr_recommended (clean and
--   free). Ranks are the 878 tie-break order.
--
-- TOKEN LIMIT (items.id=709, fail closed): every model row carries exactly
-- one output limit key in request_options. A reasoning model takes the same
-- key the docs name, no reasoning_effort is set anywhere, and output_overhead
-- 2.0 covers hidden reasoning tokens for the gpt-oss models (sample-based
-- from the Groq live check, items.id=709) and for the thinking models (an
-- estimate to be refined by per-user calibration, items.id=711). The limit
-- 4096 is a QR chat-sized budget matching the one live-verified value. It is
-- not a documented provider maximum. Where the docs name the key it is
-- recorded below. Where they do not, max_tokens (accepted by the chat
-- format everywhere) is used and flagged as unverified.
--
-- CORRECTIONS APPLIED (items.id=724 plan adjudication): Ollama Cloud stores
-- its published pay-as-you-go prices and its free credits live in
-- free_tier. Anthropic stores the Haiku prompt-length tiers in pricing and
-- token_factor 1.3.
--
-- OPENROUTER MODEL ROW (Chat-PM adjudication of the first items.id=724
-- return). The default is apodex/apodex-1.1-mini:free. It passed ONE live
-- request with provider.data_collection deny on 2026-10-11, made by Jason
-- with his own key, so an endpoint for it exists under deny. That call shows
-- the request is accepted, it does not show how the model provider handles
-- the data, which stays UNVERIFIED (verification_status unverified and the
-- disclosure in documentation_gate say so). Context 262144 comes from the
-- OpenRouter model list read on 2026-10-11. Price is 0 and 0. The deny option
-- sits in request_options beside the token limit. output_overhead 2.0 is an
-- unmeasured estimate (a reasoning-first model). Note for items.id=709-713:
-- request_options live on model rows, so an uncurated OpenRouter model
-- picked from the live list would carry no deny option. A provider-level
-- options field is needed first.
--
-- PER-MODEL PRIVACY OVERRIDES (decisions.id=878 point 5): the seeded models
-- are not affected by any of the listed exceptions, so the three override
-- columns stay NULL (inherit). The rules are recorded as text in each
-- provider documentation_gate (model_overrides): Mistral labs and preview
-- models train, DeepInfra Google and Anthropic models follow those
-- companies, OpenRouter models follow their upstream provider, Anthropic
-- Covered Models retain 30 days, Fireworks Responses API stores 30 days.
--
-- NOT VERIFIED (named in the handoff): Gemini max_tokens acceptance by the
-- compat layer (not on its page) and its default thinking level, the
-- Cloudflare API token page URL, the Mistral deep link (only the console
-- root is documented), models_path for providers whose docs show no list
-- endpoint (left NULL), Cloudflare and Mistral and Gemini and SambaNova
-- max output, jurisdiction where no page states it (left NULL), SambaNova
-- and Mistral and OpenRouter attributes (verification_status unverified).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no line inside a string literal may
-- end with a semicolon and no semicolons inside string literals.

-- ---------------------------------------------------------------------------
-- Reconcile the existing rows (Groq keeps its live-verified values)
-- ---------------------------------------------------------------------------

UPDATE providers SET
    logo_key = 'groq',
    documentation_gate = json_set(documentation_gate,
        '$.verified_on', '2026-10-11',
        '$.disclosures', json('["Groq terms are written for developers and businesses and say the service is not for consumer use.","Groq may keep inputs and outputs for up to 30 days to troubleshoot reliability or investigate abuse, unless zero data retention is turned on in the Groq console."]'),
        '$.model_overrides', json('["Groq batch and fine-tuning features retain data, inference does not."]')),
    last_reviewed_at = '2026-10-11'
WHERE id = 'groq';

UPDATE providers SET
    logo_key = 'mistral',
    credential_acquisition = '{"method":"guided_paste","auth_url":null,"key_pattern":null,"keys_page_url":"https://console.mistral.ai"}',
    documentation_gate = json_set(documentation_gate,
        '$.verified_on', '2026-10-11',
        '$.disclosures', json('["Mistral commercial terms are written for organizations.","On the free Experiment plan Mistral may use your API requests to train its models unless Anonymous improvement data is turned off in the Mistral Admin panel under Privacy. QR cannot check that setting."]'),
        '$.model_overrides', json('["Mistral labs and preview models always train on your data.","The Vibe and API training toggles are separate."]')),
    last_reviewed_at = '2026-10-11'
WHERE id = 'mistral';

-- Mistral Small 4 (mistral-small-2603): the model page gives context 256k
-- and prices of 0.15 and 0.6 USD per million (it does not label which is
-- input) and no maximum output. The API reference names max_tokens as the
-- output limit parameter and states no default. 4096 replaces the
-- unverified 2048 and is a curated chat budget, not a documented maximum.
UPDATE provider_models
   SET request_options = '{"max_tokens":4096}',
       pricing = json_set(pricing, '$.note', 'The Mistral model page lists 0.15 and 0.6 USD per million tokens without labeling which is input and which is output. The input and output order is unverified.')
 WHERE id = 'mistral:mistral-small-2603';

-- ---------------------------------------------------------------------------
-- Ollama Cloud (rank 2, clean, recommended)
-- docs.ollama.com/api/openai-compatibility: base https://ollama.com/v1,
-- max_tokens supported, /v1/models present, reasoning_effort supported.
-- ollama.com/pricing: gpt-oss:20b 0.07 input, 0.035 cached, 0.30 output.
-- Free plan: starter usage credits, 1 concurrent request.
-- ---------------------------------------------------------------------------

INSERT OR IGNORE INTO providers
    (id, display_name, provider_type, mode, launch_url, activation_status,
     documentation_gate, last_reviewed_at, review_trigger_note, created_at,
     is_local, login_required, is_anonymous, retains_data,
     trains_on_data_by_default, privacy_guardian_default_level, risk_rating,
     user_privacy_summary, qr_recommended, privacy_commitment_basis,
     qr_recommendation_rank, api_dialect, base_url_template, auth_scheme,
     extra_headers, models_path, user_variables, free_tier, plan_inclusion,
     retention_days, jurisdiction, verification_status, credential_acquisition,
     spend_limit_url, logo_key)
VALUES
    ('ollama_cloud', 'Ollama Cloud', 'cloud_inference_api', 'api', NULL, 'active',
     '{"pending_curation":false,"summary":"Ollama Terms of Service (May 2026) state that Ollama does not use inputs or outputs to train AI models and takes only a limited license to process content to provide the service. The privacy policy says cloud prompts and responses are processed transiently and not stored beyond the time needed, with measures designed to minimize retention. The pricing FAQ says they are never logged or trained on. Hosting is mainly in the United States and may route to Europe, with NVIDIA Cloud Providers required to keep no logs and no retention.","jurisdiction":"United States (Ollama Inc., California law)","sources":["https://ollama.com/privacy","https://docs.ollama.com/cloud","https://ollama.com/pricing"],"verified_on":"2026-10-11","disclosures":["Ollama privacy policy says cloud requests are designed to minimize retention, while its pricing FAQ says they are never logged. Both say they are never used for training."],"model_overrides":[]}',
     '2026-10-11',
     'Curated 2026-10-11, items.id=724 from items.id=723 findings and the Ollama docs and pricing pages. Re-check base URL, model list, prices and the free-credit model set at each pre-release provider check.',
     datetime('now'),
     0, 1, 0, 0, 0, 'low', 1,
     '{"summary": "Ollama does not use your prompts or responses to train its models, and says cloud requests are processed transiently rather than stored.", "account_status_note": "Its privacy policy and its pricing FAQ describe retention in slightly different words, so the strongest claim (never logged) rests on the FAQ.", "actions": [], "settings_url": "https://ollama.com/privacy"}',
     1, 'contractual', 2,
     'openai_chat', 'https://ollama.com/v1', 'bearer', '{}', '/models', '[]',
     '{"available":true,"requires_card":null,"limits":{"rpm":null,"rpd":null,"tpm":null,"tpd":null},"concurrent_requests":1,"credits":"Starter usage credits, dollar amount not published (a percentage meter that refills every 4 weeks)","applies_to":"gemma4:31b, gpt-oss:120b, gpt-oss:20b, nemotron-3-nano:30b, nemotron-3-super, nemotron-3-ultra","as_of":"2026-10-11","source_url":"https://ollama.com/pricing"}',
     '[]',
     NULL, 'United States', 'verified',
     '{"method":"local_signin","auth_url":null,"key_pattern":null,"keys_page_url":"https://ollama.com/settings/keys"}',
     NULL, 'ollama');

INSERT INTO provider_models
    (id, provider_id, model_id, context_window_tokens, is_default, created_at,
     pricing, request_options, output_overhead, status, qr_recommended,
     qr_recommendation_rank, context_window_source, context_window_as_of)
VALUES
    ('ollama_cloud:gpt-oss:20b', 'ollama_cloud', 'gpt-oss:20b', 131072, 1, datetime('now'),
     '{"input_per_mtok":0.07,"output_per_mtok":0.30,"currency":"USD","cache":null,"tiers":null,"as_of":"2026-10-11","source_url":"https://ollama.com/pricing","source":"curated","note":"Pay-as-you-go price. Cached input 0.035. The free plan covers this model with starter credits."}',
     '{"max_tokens":4096}', 2.0, 'active', 1, 1, 'curated', '2026-10-11');

-- ---------------------------------------------------------------------------
-- Cloudflare Workers AI (rank 3, clean with retention unstated, recommended)
-- developers.cloudflare.com/workers-ai/configuration/open-ai-compatibility
-- gives the account-scoped base URL. The model page lists max_tokens
-- (default 256), context 128,000, 0.20 input and 0.30 output per million.
-- No list endpoint is documented on the compat page, so models_path is NULL.
-- ---------------------------------------------------------------------------

INSERT OR IGNORE INTO providers
    (id, display_name, provider_type, mode, launch_url, activation_status,
     documentation_gate, last_reviewed_at, review_trigger_note, created_at,
     is_local, login_required, is_anonymous, retains_data,
     trains_on_data_by_default, privacy_guardian_default_level, risk_rating,
     user_privacy_summary, qr_recommended, privacy_commitment_basis,
     qr_recommendation_rank, api_dialect, base_url_template, auth_scheme,
     extra_headers, models_path, user_variables, free_tier, plan_inclusion,
     retention_days, jurisdiction, verification_status, credential_acquisition,
     spend_limit_url, logo_key)
VALUES
    ('cloudflare_workers_ai', 'Cloudflare Workers AI', 'cloud_inference_api', 'api', NULL, 'active',
     '{"pending_curation":false,"summary":"Cloudflare data usage page (updated 2026-04-21): customer content is not made available to other customers and is not used to train models or improve Cloudflare or third-party services without explicit consent. Content may be stored if you use a storage service. Cloudflare neither creates nor trains the models. Inference retention and inference location are not stated anywhere QR could find.","jurisdiction":"Not stated","sources":["https://developers.cloudflare.com/workers-ai/platform/data-usage/","https://developers.cloudflare.com/workers-ai/platform/pricing/","https://developers.cloudflare.com/workers-ai/configuration/open-ai-compatibility/"],"verified_on":"2026-10-11","disclosures":["Cloudflare does not say how long it keeps inference requests or where they are processed. It states that your content is not used to train models without your explicit consent."],"model_overrides":[]}',
     '2026-10-11',
     'Curated 2026-10-11, items.id=724 from items.id=723 findings and the Cloudflare docs. Re-check the retention statement, free allocation and prices at each pre-release provider check.',
     datetime('now'),
     0, 1, 0, 1, 0, 'low', 1,
     '{"summary": "Cloudflare says it does not use your content to train models without your explicit consent.", "account_status_note": "Cloudflare does not state how long inference requests are kept or where they are processed.", "actions": [], "settings_url": "https://developers.cloudflare.com/workers-ai/platform/data-usage/"}',
     1, NULL, 3,
     'openai_chat', 'https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1', 'bearer', '{}', NULL, '["account_id"]',
     '{"available":true,"requires_card":null,"limits":{"rpm":300,"rpd":null,"tpm":null,"tpd":null},"neurons_per_day":10000,"paid_only_models_rpm":20,"applies_to":"text generation models that do not require a paid billing method","as_of":"2026-10-11","source_url":"https://developers.cloudflare.com/workers-ai/platform/pricing/"}',
     '[]',
     NULL, NULL, 'verified',
     '{"method":"guided_paste","auth_url":null,"key_pattern":null,"keys_page_url":"https://dash.cloudflare.com/profile/api-tokens"}',
     NULL, 'cloudflare');

INSERT INTO provider_models
    (id, provider_id, model_id, context_window_tokens, is_default, created_at,
     pricing, request_options, output_overhead, status, qr_recommended,
     qr_recommendation_rank, context_window_source, context_window_as_of)
VALUES
    ('cloudflare_workers_ai:@cf/openai/gpt-oss-20b', 'cloudflare_workers_ai', '@cf/openai/gpt-oss-20b', 128000, 1, datetime('now'),
     '{"input_per_mtok":0.20,"output_per_mtok":0.30,"currency":"USD","cache":null,"tiers":null,"as_of":"2026-10-11","source_url":"https://developers.cloudflare.com/workers-ai/models/gpt-oss-20b/","source":"curated","note":"The free allocation is 10000 neurons per day, about 0.37 million output tokens."}',
     '{"max_tokens":4096}', 2.0, 'active', 1, 1, 'curated', '2026-10-11');

-- ---------------------------------------------------------------------------
-- OpenRouter (rank 4, conditional, not recommended). Default model row below.
-- docs: api-reference/authentication (keys page https://openrouter.ai/keys,
-- per-key credit limit), guides/overview/auth/oauth (authorization URL
-- https://openrouter.ai/auth, PKCE S256, key exchange at the API).
-- ---------------------------------------------------------------------------

INSERT OR IGNORE INTO providers
    (id, display_name, provider_type, mode, launch_url, activation_status,
     documentation_gate, last_reviewed_at, review_trigger_note, created_at,
     is_local, login_required, is_anonymous, retains_data,
     trains_on_data_by_default, privacy_guardian_default_level, risk_rating,
     user_privacy_summary, qr_recommended, privacy_commitment_basis,
     qr_recommendation_rank, api_dialect, base_url_template, auth_scheme,
     extra_headers, models_path, user_variables, free_tier, plan_inclusion,
     retention_days, jurisdiction, verification_status, credential_acquisition,
     spend_limit_url, logo_key)
VALUES
    ('openrouter', 'OpenRouter', 'cloud_inference_api', 'api', NULL, 'active',
     '{"pending_curation":false,"summary":"OpenRouter privacy policy (updated 2026-08-31) says OpenRouter does not use inputs or outputs for training, and that some model providers may. Which provider handles a request decides its logging and training. Requests can be restricted per request with provider.data_collection deny, which routes only to providers that do not collect data. The FAQ says only request metadata is logged while the privacy policy lists inputs among the data collected, so the two documents disagree.","jurisdiction":"United States (OpenRouter Inc., New York law)","sources":["https://openrouter.ai/privacy","https://openrouter.ai/docs/guides/privacy/provider-logging","https://openrouter.ai/docs/guides/routing/provider-selection"],"verified_on":"2026-10-11","disclosures":["OpenRouter FAQ says prompts are not logged, but its privacy policy lists inputs among the data it collects. The two disagree.","The model provider that handles your request decides logging and training. QR asks OpenRouter to use only providers that do not collect data, and one free model passed a live request with that restriction on 2026-10-11, but how its provider handles your data is unverified."],"model_overrides":["Every OpenRouter model follows its upstream provider policy."],"required_request_options":{"provider":{"data_collection":"deny"}},"model_notes":["The default model passed one live data_collection deny request on 2026-10-11 made by Jason. Its provider actual data handling is unverified."]}',
     '2026-10-11',
     'Curated 2026-10-11, items.id=724. Default model passed one live deny request on 2026-10-11 (Jason), provider data handling unverified. Re-check the policy conflict, the free model and its endpoints at each pre-release provider check.',
     datetime('now'),
     0, 1, 0, 1, 0, 'medium', 2,
     '{"summary": "OpenRouter itself does not train on your prompts, but the company that runs the model you pick may log or train on them.", "account_status_note": "OpenRouter FAQ and privacy policy disagree about whether prompts are logged. QR asks OpenRouter to use only providers that do not collect data, but how the provider behind the default free model handles your data is unverified.", "actions": [], "settings_url": "https://openrouter.ai/privacy"}',
     0, NULL, 4,
     'openai_chat', 'https://openrouter.ai/api/v1', 'bearer', '{}', '/models', '[]',
     '{"available":true,"requires_card":null,"limits":{"rpm":20,"rpd":50,"tpm":null,"tpd":null},"rpd_with_10_usd_credits":1000,"applies_to":"models whose id ends with :free","as_of":"2026-10-11","source_url":"https://openrouter.ai/docs/api-reference/limits"}',
     '[]',
     NULL, 'United States', 'unverified',
     '{"method":"oauth_pkce","auth_url":"https://openrouter.ai/auth","key_pattern":null,"keys_page_url":"https://openrouter.ai/keys"}',
     'https://openrouter.ai/keys', 'openrouter');

INSERT INTO provider_models
    (id, provider_id, model_id, context_window_tokens, is_default, created_at,
     pricing, request_options, output_overhead, status, qr_recommended,
     qr_recommendation_rank, context_window_source, context_window_as_of)
VALUES
    ('openrouter:apodex/apodex-1.1-mini:free', 'openrouter', 'apodex/apodex-1.1-mini:free', 262144, 1, datetime('now'),
     '{"input_per_mtok":0,"output_per_mtok":0,"currency":"USD","cache":null,"tiers":null,"as_of":"2026-10-11","source_url":"https://openrouter.ai/collections/free-models","source":"curated","note":"Free model, 20 requests per minute and 50 per day. output_overhead 2.0 is an unmeasured estimate (a reasoning-first model). Passed one live data_collection deny request on 2026-10-11 (Jason). Its provider actual data handling is unverified."}',
     '{"max_tokens":4096,"provider":{"data_collection":"deny"}}', 2.0, 'active', 1, 1, 'curated', '2026-10-11');

-- ---------------------------------------------------------------------------
-- Gemini API (rank 6, strong warn on the free tier, not recommended)
-- ai.google.dev: OpenAI compat base with trailing slash, /models list,
-- gemini-3.1-flash-lite stable (input 1,048,576, output 65,536), paid price
-- 0.25 input (text) and 1.50 output, free of charge on the free tier.
-- max_output_tokens includes thinking tokens. The compat page does not name
-- max_tokens, so the key is unverified.
-- ---------------------------------------------------------------------------

INSERT OR IGNORE INTO providers
    (id, display_name, provider_type, mode, launch_url, activation_status,
     documentation_gate, last_reviewed_at, review_trigger_note, created_at,
     is_local, login_required, is_anonymous, retains_data,
     trains_on_data_by_default, privacy_guardian_default_level, risk_rating,
     user_privacy_summary, qr_recommended, privacy_commitment_basis,
     qr_recommendation_rank, api_dialect, base_url_template, auth_scheme,
     extra_headers, models_path, user_variables, free_tier, plan_inclusion,
     retention_days, jurisdiction, verification_status, credential_acquisition,
     spend_limit_url, logo_key)
VALUES
    ('gemini_api', 'Gemini API', 'cloud_inference_api', 'api', NULL, 'active',
     '{"pending_curation":false,"summary":"Gemini API Additional Terms (effective 2026-03-23, read in full 2026-10-11). Unpaid services: Google uses submitted content and responses to improve its products and machine-learning technologies, and human reviewers may read and annotate input and output after it is disconnected from the account. Paid services (a Cloud project with active billing): Google does not use prompts or responses to improve its products, processes them under its data processing addendum, and logs them for a limited unspecified period to detect abuse and meet legal disclosures. Data may be stored or cached in any country where Google operates. The OpenAI-compatible layer is marked beta.","jurisdiction":"United States (Google), processing may occur in any country","sources":["https://ai.google.dev/gemini-api/terms","https://ai.google.dev/gemini-api/docs/openai","https://ai.google.dev/gemini-api/docs/pricing"],"verified_on":"2026-10-11","disclosures":["Google terms say the Gemini API is for developers building for professional or business purposes, not consumer use.","On the free tier Google uses what you send to improve its products and human reviewers may read it. The paid tier does not use your data that way.","The free tier may not be used for apps that serve people in the EEA, Switzerland or the UK."],"model_overrides":[]}',
     '2026-10-11',
     'Curated 2026-10-11, items.id=724 from items.id=723 findings and the Gemini docs. Re-check terms, free-tier limits, model ids and the beta compat layer at each pre-release provider check.',
     datetime('now'),
     0, 1, 0, 1, 1, 'high', 3,
     '{"summary": "On the free tier Google may use what you send to improve its products, and people may read it. The paid tier does not.", "account_status_note": "Google terms bar the free tier for apps that serve people in the EEA, Switzerland or the UK.", "actions": [], "settings_url": "https://ai.google.dev/gemini-api/terms"}',
     0, NULL, 6,
     'openai_chat', 'https://generativelanguage.googleapis.com/v1beta/openai/', 'bearer', '{}', '/models', '[]',
     '{"available":true,"requires_card":false,"limits":{"rpm":15,"rpd":500,"tpm":250000,"tpd":null},"applies_to":"gemini-3.1-flash-lite","as_of":"2026-10-11","source_url":"https://ai.google.dev/gemini-api/docs/rate-limits"}',
     '[]',
     NULL, 'United States', 'verified',
     '{"method":"guided_paste","auth_url":null,"key_pattern":null,"keys_page_url":"https://aistudio.google.com/apikey"}',
     NULL, 'gemini');

INSERT INTO provider_models
    (id, provider_id, model_id, context_window_tokens, is_default, created_at,
     pricing, request_options, output_overhead, status, qr_recommended,
     qr_recommendation_rank, context_window_source, context_window_as_of)
VALUES
    ('gemini_api:gemini-3.1-flash-lite', 'gemini_api', 'gemini-3.1-flash-lite', 1048576, 1, datetime('now'),
     '{"input_per_mtok":0.25,"output_per_mtok":1.50,"currency":"USD","cache":null,"tiers":null,"as_of":"2026-10-11","source_url":"https://ai.google.dev/gemini-api/docs/pricing","source":"curated","note":"Paid tier price for text input. Audio input is 0.50. The free tier is free of charge. output_overhead 2.0 is an unmeasured estimate: thinking tokens count toward the output limit and the default thinking level is not documented."}',
     '{"max_tokens":4096}', 2.0, 'active', 1, 1, 'curated', '2026-10-11');

-- ---------------------------------------------------------------------------
-- SambaNova (rank 7, unverified, not recommended)
-- docs.sambanova.ai get-started/api-keys-urls: base https://api.sambanova.ai/v1,
-- keys at https://cloud.sambanova.ai/apis. cloud.sambanova.ai/plans/pricing:
-- gpt-oss-120b 0.22 input and 0.59 output. Model list: gpt-oss-120b, 128k.
-- Free tier from docs.sambanova.ai models/rate-limits (items.id=704).
-- ---------------------------------------------------------------------------

INSERT OR IGNORE INTO providers
    (id, display_name, provider_type, mode, launch_url, activation_status,
     documentation_gate, last_reviewed_at, review_trigger_note, created_at,
     is_local, login_required, is_anonymous, retains_data,
     trains_on_data_by_default, privacy_guardian_default_level, risk_rating,
     user_privacy_summary, qr_recommended, privacy_commitment_basis,
     qr_recommendation_rank, api_dialect, base_url_template, auth_scheme,
     extra_headers, models_path, user_variables, free_tier, plan_inclusion,
     retention_days, jurisdiction, verification_status, credential_acquisition,
     spend_limit_url, logo_key)
VALUES
    ('sambanova', 'SambaNova', 'cloud_inference_api', 'api', NULL, 'active',
     '{"pending_curation":false,"summary":"SambaNova contract terms grant processing solely to provide the service. No retention period is stated. A SambaNova staff reply on its community forum says prompts are not stored or used for training by default, but that is not a primary source and QR could not confirm it from the terms.","jurisdiction":"Not stated","sources":["https://docs.sambanova.ai/docs/en/models/rate-limits","https://docs.sambanova.ai/docs/en/get-started/api-keys-urls"],"verified_on":"2026-10-11","disclosures":["SambaNova does not state how long it keeps requests. A staff forum reply says prompts are not stored or used for training, but QR could not confirm that from SambaNova terms."],"model_overrides":[]}',
     '2026-10-11',
     'Curated 2026-10-11, items.id=724 from items.id=723 findings. Unverified posture. Re-check the terms and free quota at each pre-release provider check.',
     datetime('now'),
     0, 1, 0, 1, 0, 'high', 3,
     '{"summary": "SambaNova says it processes your requests only to provide the service, but it does not state how long it keeps them, and QR could not confirm its training policy from its terms.", "account_status_note": "Treat this provider as unverified.", "actions": [], "settings_url": "https://docs.sambanova.ai/"}',
     0, NULL, 7,
     'openai_chat', 'https://api.sambanova.ai/v1', 'bearer', '{}', NULL, '[]',
     '{"available":true,"requires_card":false,"limits":{"rpm":20,"rpd":20,"tpm":null,"tpd":200000},"applies_to":"gpt-oss-120b, Meta-Llama-3.3-70B-Instruct and DeepSeek-V3.1 while no payment method is linked","as_of":"2026-10-11","source_url":"https://docs.sambanova.ai/docs/en/models/rate-limits"}',
     '[]',
     NULL, NULL, 'unverified',
     '{"method":"guided_paste","auth_url":null,"key_pattern":null,"keys_page_url":"https://cloud.sambanova.ai/apis"}',
     NULL, 'sambanova');

INSERT INTO provider_models
    (id, provider_id, model_id, context_window_tokens, is_default, created_at,
     pricing, request_options, output_overhead, status, qr_recommended,
     qr_recommendation_rank, context_window_source, context_window_as_of)
VALUES
    ('sambanova:gpt-oss-120b', 'sambanova', 'gpt-oss-120b', 131072, 1, datetime('now'),
     '{"input_per_mtok":0.22,"output_per_mtok":0.59,"currency":"USD","cache":null,"tiers":null,"as_of":"2026-10-11","source_url":"https://cloud.sambanova.ai/plans/pricing","source":"curated","note":"The docs give the context as 128k, stored as 131072."}',
     '{"max_tokens":4096}', 2.0, 'active', 1, 1, 'curated', '2026-10-11');

-- ---------------------------------------------------------------------------
-- Anthropic API (rank 8, clean and metered, anthropic_messages dialect)
-- platform.claude.com docs: base https://api.anthropic.com (the dialect
-- appends /v1/messages), x-api-key, models at /v1/models, claude-haiku-5-5
-- (a pinned snapshot, context 1M, max output 128K, thinking on by default
-- and counted in max_tokens). Haiku 5.5 is priced by prompt length. A newer
-- tokenizer produces about 30 percent more tokens, so token_factor is 1.3.
-- Researcher disclosure: this row was curated by Claude, made by Anthropic,
-- from Anthropic public pages read directly. An independent check was
-- waived by Jason on 2026-10-11.
-- ---------------------------------------------------------------------------

INSERT OR IGNORE INTO providers
    (id, display_name, provider_type, mode, launch_url, activation_status,
     documentation_gate, last_reviewed_at, review_trigger_note, created_at,
     is_local, login_required, is_anonymous, retains_data,
     trains_on_data_by_default, privacy_guardian_default_level, risk_rating,
     user_privacy_summary, qr_recommended, privacy_commitment_basis,
     qr_recommendation_rank, api_dialect, base_url_template, auth_scheme,
     extra_headers, models_path, user_variables, free_tier, plan_inclusion,
     retention_days, jurisdiction, verification_status, credential_acquisition,
     spend_limit_url, logo_key)
VALUES
    ('anthropic_api', 'Anthropic API', 'cloud_inference_api', 'api', NULL, 'active',
     '{"pending_curation":false,"summary":"Anthropic privacy center: commercial products and the API are not used for training by default, except feedback you submit. Inputs and outputs are deleted from the backend within 30 days, except for longer retention under your control, zero data retention agreements, Usage Policy enforcement and legal requirements. Content flagged as a Usage Policy violation is kept up to 2 years and classifier scores up to 7 years. Covered Models (Mythos-class) retain prompts and outputs for 30 days for safety, which for standard API users is the same as the normal period.","jurisdiction":"United States","sources":["https://privacy.claude.com/en/articles/7996866","https://privacy.claude.com/en/articles/7996868","https://privacy.claude.com/en/articles/15425996"],"verified_on":"2026-10-11","verification_note":"Curated by Claude, made by Anthropic, from Anthropic public pages read directly. An independent check was waived on 2026-10-11.","disclosures":["Anthropic keeps inputs and outputs for up to 30 days, and longer for content flagged under its usage policy."],"model_overrides":["Covered Models retain prompts and outputs for 30 days on every platform."]}',
     '2026-10-11',
     'Curated 2026-10-11, items.id=724 from items.id=723 findings and Anthropic docs. Re-check prices, the prompt-length tiers, the model lifecycle (the models API returns retires_at) and the tokenizer note at each pre-release provider check.',
     datetime('now'),
     0, 1, 0, 1, 0, 'low', 1,
     '{"summary": "Anthropic does not use API content to train its models by default and deletes it from its backend within 30 days.", "account_status_note": "Content flagged under the usage policy can be kept much longer.", "actions": [], "settings_url": "https://privacy.claude.com/"}',
     0, NULL, 8,
     'anthropic_messages', 'https://api.anthropic.com', 'x-api-key', '{}', '/v1/models', '[]',
     '{"available":false,"requires_card":null,"limits":null,"applies_to":null,"as_of":"2026-10-11","source_url":"https://platform.claude.com/docs/en/api/rate-limits"}',
     '[]',
     30, 'United States', 'verified',
     '{"method":"guided_paste","auth_url":null,"key_pattern":null,"keys_page_url":"https://platform.claude.com/settings/keys"}',
     'https://platform.claude.com/settings/billing', 'anthropic');

INSERT INTO provider_models
    (id, provider_id, model_id, context_window_tokens, is_default, created_at,
     pricing, request_options, token_factor, output_overhead, status,
     requires_paid, qr_recommended, qr_recommendation_rank,
     context_window_source, context_window_as_of)
VALUES
    ('anthropic_api:claude-haiku-5-5', 'anthropic_api', 'claude-haiku-5-5', 1000000, 1, datetime('now'),
     '{"input_per_mtok":0.10,"output_per_mtok":0.50,"currency":"USD","cache":null,"tiers":[{"prompt_tokens_up_to":100000,"input_per_mtok":0.10,"output_per_mtok":0.50},{"prompt_tokens_over":100000,"input_per_mtok":0.50,"output_per_mtok":2.50}],"as_of":"2026-10-11","source_url":"https://platform.claude.com/docs/en/about-claude/pricing","source":"curated","note":"The prompt length counts all input tokens including cache reads and writes. Each request is priced on its own. output_overhead 2.0 is an unmeasured estimate: the docs say thinking is on by default for this model with no thinking field in the request, and thinking tokens count toward max_tokens."}',
     '{"max_tokens":4096}', 1.3, 2.0, 'active', 1, 1, 1, 'curated', '2026-10-11');

-- ---------------------------------------------------------------------------
-- Fireworks (rank 9, clean and metered)
-- docs.fireworks.ai tools-sdks/openai-compatibility: base
-- https://api.fireworks.ai/inference/v1, no list endpoint documented.
-- The gpt-oss-20b model page says serverless is not supported, so the
-- default is gpt-oss-120b (serverless supported, 131k, 0.15 and 0.60,
-- docs.fireworks.ai/serverless/pricing). QR must use chat completions
-- only: the Responses API stores conversations for 30 days.
-- ---------------------------------------------------------------------------

INSERT OR IGNORE INTO providers
    (id, display_name, provider_type, mode, launch_url, activation_status,
     documentation_gate, last_reviewed_at, review_trigger_note, created_at,
     is_local, login_required, is_anonymous, retains_data,
     trains_on_data_by_default, privacy_guardian_default_level, risk_rating,
     user_privacy_summary, qr_recommended, privacy_commitment_basis,
     qr_recommendation_rank, api_dialect, base_url_template, auth_scheme,
     extra_headers, models_path, user_variables, free_tier, plan_inclusion,
     retention_days, jurisdiction, verification_status, credential_acquisition,
     spend_limit_url, logo_key)
VALUES
    ('fireworks', 'Fireworks', 'cloud_inference_api', 'api', NULL, 'active',
     '{"pending_curation":false,"summary":"Fireworks data handling page: zero data retention by default for open models without explicit opt-in. Prompt data sits in volatile memory for the request, and with prompt caching for several minutes. Request metadata is logged. The Responses API stores conversations for 30 days when store is true, which is its default, so QR must use chat completions only.","jurisdiction":"Not stated","sources":["https://docs.fireworks.ai/guides/security_compliance/data_handling","https://docs.fireworks.ai/serverless/pricing"],"verified_on":"2026-10-11","disclosures":["Fireworks logs request metadata. It keeps no prompt data for open models by default."],"model_overrides":["The Fireworks Responses API stores conversations for 30 days. QR uses chat completions only."]}',
     '2026-10-11',
     'Curated 2026-10-11, items.id=724 from items.id=723 findings and the Fireworks docs. Re-check the serverless model list and prices at each pre-release provider check.',
     datetime('now'),
     0, 1, 0, 0, 0, 'low', 1,
     '{"summary": "Fireworks keeps no prompt data for open models by default and does not train on it, but it logs request metadata.", "account_status_note": "QR only uses the chat endpoint, because Fireworks stores conversations from its Responses API for 30 days.", "actions": [], "settings_url": "https://docs.fireworks.ai/guides/security_compliance/data_handling"}',
     0, NULL, 9,
     'openai_chat', 'https://api.fireworks.ai/inference/v1', 'bearer', '{}', NULL, '[]',
     '{"available":false,"requires_card":null,"limits":null,"applies_to":null,"as_of":"2026-10-11","source_url":"https://docs.fireworks.ai/serverless/pricing"}',
     '[]',
     NULL, NULL, 'verified',
     '{"method":"guided_paste","auth_url":null,"key_pattern":null,"keys_page_url":"https://app.fireworks.ai/settings/users/api-keys"}',
     NULL, 'fireworks');

INSERT INTO provider_models
    (id, provider_id, model_id, context_window_tokens, is_default, created_at,
     pricing, request_options, output_overhead, status, requires_paid,
     qr_recommended, qr_recommendation_rank, context_window_source,
     context_window_as_of)
VALUES
    ('fireworks:accounts/fireworks/models/gpt-oss-120b', 'fireworks', 'accounts/fireworks/models/gpt-oss-120b', 131072, 1, datetime('now'),
     '{"input_per_mtok":0.15,"output_per_mtok":0.60,"currency":"USD","cache":null,"tiers":null,"as_of":"2026-10-11","source_url":"https://docs.fireworks.ai/serverless/pricing","source":"curated","note":"Cached input 0.015. The model page gives the context as 131k in one place and 128K in another."}',
     '{"max_tokens":4096}', 2.0, 'active', 1, 1, 1, 'curated', '2026-10-11');

-- ---------------------------------------------------------------------------
-- DeepInfra (rank 10, clean and metered)
-- docs.deepinfra.com/chat/overview: base https://api.deepinfra.com/v1/openai,
-- max_tokens documented (hard cap 16384 for most models), no list endpoint
-- documented. deepinfra.com/openai/gpt-oss-20b: 131,072 context, 0.03 input
-- and 0.14 output, Public, Zero retention.
-- ---------------------------------------------------------------------------

INSERT OR IGNORE INTO providers
    (id, display_name, provider_type, mode, launch_url, activation_status,
     documentation_gate, last_reviewed_at, review_trigger_note, created_at,
     is_local, login_required, is_anonymous, retains_data,
     trains_on_data_by_default, privacy_guardian_default_level, risk_rating,
     user_privacy_summary, qr_recommended, privacy_commitment_basis,
     qr_recommendation_rank, api_dialect, base_url_template, auth_scheme,
     extra_headers, models_path, user_variables, free_tier, plan_inclusion,
     retention_days, jurisdiction, verification_status, credential_acquisition,
     spend_limit_url, logo_key)
VALUES
    ('deepinfra', 'DeepInfra', 'cloud_inference_api', 'api', NULL, 'active',
     '{"pending_curation":false,"summary":"DeepInfra data privacy page: inputs are not stored to disk and outputs are deleted after the response. Submitted data is not used for training, except for Google and Anthropic models, where the receiving company policy applies and those companies may log prompts for a limited period. US data centers.","jurisdiction":"United States","sources":["https://docs.deepinfra.com/account/data-privacy","https://deepinfra.com/openai/gpt-oss-20b"],"verified_on":"2026-10-11","disclosures":["DeepInfra does not store inputs and deletes outputs after the response, except for Google and Anthropic models, which follow those companies own policies."],"model_overrides":["DeepInfra Google and Anthropic models follow those companies policies."]}',
     '2026-10-11',
     'Curated 2026-10-11, items.id=724 from items.id=723 findings and the DeepInfra docs. Re-check prices and the model list at each pre-release provider check.',
     datetime('now'),
     0, 1, 0, 0, 0, 'low', 1,
     '{"summary": "DeepInfra does not store your inputs, deletes outputs after the response, and does not train on your data.", "account_status_note": "Google and Anthropic models served through DeepInfra follow those companies own data policies.", "actions": [], "settings_url": "https://docs.deepinfra.com/account/data-privacy"}',
     0, NULL, 10,
     'openai_chat', 'https://api.deepinfra.com/v1/openai', 'bearer', '{}', NULL, '[]',
     '{"available":false,"requires_card":null,"limits":null,"applies_to":null,"as_of":"2026-10-11","source_url":"https://deepinfra.com/openai/gpt-oss-20b"}',
     '[]',
     0, 'United States', 'verified',
     '{"method":"guided_paste","auth_url":null,"key_pattern":null,"keys_page_url":"https://deepinfra.com/dash/api_keys"}',
     NULL, 'deepinfra');

INSERT INTO provider_models
    (id, provider_id, model_id, context_window_tokens, is_default, created_at,
     pricing, request_options, output_overhead, status, requires_paid,
     qr_recommended, qr_recommendation_rank, context_window_source,
     context_window_as_of)
VALUES
    ('deepinfra:openai/gpt-oss-20b', 'deepinfra', 'openai/gpt-oss-20b', 131072, 1, datetime('now'),
     '{"input_per_mtok":0.03,"output_per_mtok":0.14,"currency":"USD","cache":null,"tiers":null,"as_of":"2026-10-11","source_url":"https://deepinfra.com/openai/gpt-oss-20b","source":"curated","note":"Standard tier. Priority is 1.5x and flex is 0.8x."}',
     '{"max_tokens":4096}', 2.0, 'active', 1, 1, 1, 'curated', '2026-10-11');

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (28, datetime('now'),
    'items.id=724: curated R1 hosted providers as data (decisions.id=878). Eight providers added (ollama_cloud, cloudflare_workers_ai, openrouter, gemini_api, sambanova, anthropic_api, fireworks, deepinfra), groq and mistral reconciled (disclosures, logo keys, Mistral key page and a 4096 token limit), eight new default model rows with prices and limits read 2026-10-11 from provider docs pages, all with an output token limit in request_options. openrouter default is apodex/apodex-1.1-mini:free with the deny option in request_options (one live deny call by Jason on 2026-10-11, provider data handling unverified)');
