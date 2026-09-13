-- shared_018.sql
--
-- items.id=347/486-adjacent provider-catalog work (2026-09-12, Jason/Chat-PM):
-- (1) preference_tier column on providers -- QR's own curation judgment,
--     'preferred' (recommended, default-visible once any onboarding/
--     confirmation step passes) vs. 'supported' (vetted and usable, not
--     QR-recommended -- hidden from the Tier 3 screen unless a user
--     specifically requests it). Open vocabulary, no CHECK -- matches
--     provider_type's own precedent (a small, evolving classification,
--     not a fixed domain). Provider-level fact only -- a user's personal
--     approval of a 'supported' provider for their own use is a separate,
--     per-user/per-install concern, deferred to the Tier 3 onboarding/
--     settings design session, not stored here.
-- (2) duckai/claude/chatgpt/gemini marked preference_tier='preferred' --
--     the existing four fully-vetted Tier 3 external_service providers.
--     groq/mistral (Tier 1.5, provider_type='cloud_inference_api') are
--     NOT part of this Tier 3 preference_tier assignment at all -- they
--     stay at the column's DEFAULT 'supported', which is not their
--     operative classification (Tier 1.5 has its own separate selection
--     mechanism, provider_type='cloud_inference_api' filtering) but is
--     harmless since nothing currently reads preference_tier for
--     provider_type != 'external_service'/'split_screen_web' rows.
-- (3) Three NEW providers rows for Tier 3 web-chat access -- Groq and
--     Mistral's own official chat webpages (genuinely distinct access
--     surfaces from their existing Tier 1.5 API rows, which are untouched
--     by this migration), plus DeepSeek (items.id=347's completed privacy
--     assessment: clears decisions.id=710's documentation-gate criteria,
--     but sits meaningfully weaker than every existing Tier 3 provider on
--     jurisdiction/retention/training-use -- Jason's call to add as
--     'supported', not 'preferred', matching that finding). All three:
--     provider_type='external_service' (all require login/account, none
--     fit Tier 2's anonymous split_screen_web shape -- see
--     commands/tier3_pane.rs::lane_str()), mode='embedded_web',
--     login_required=1, preference_tier='supported'.
--
-- IDS ARE NON-SEMANTIC BY CONVENTION AS OF THIS MIGRATION (Jason,
-- 2026-09-12): providers.id must never be parsed or pattern-matched by
-- application code to infer capability, access mode, or ownership --
-- display_name carries 100% of human-facing identity, and every
-- selection decision must be made via real columns (provider_type, mode,
-- preference_tier, etc.), never by matching on id shape. This migration's
-- three new ids (groqchat, mistralvibe, deepseek) are plain, arbitrary
-- slugs chosen for human legibility only, not because the id string
-- itself is meaningful -- e.g. groqchat's relationship to Tier 3 is
-- established by its provider_type/mode columns, not by its id
-- containing the word "chat". A later Chat-DEV/Code pass should audit
-- whether any existing code path (see conductor/executor.rs's `Some(
-- "mistral") | Some("groq")` doc-comment references, groq.rs/mistral.rs's
-- provider_id() literals) violates this going forward -- not retroactively
-- enforced on the pre-existing groq/mistral (Tier 1.5) rows by this
-- migration, which are left untouched.
--
-- DISPLAY_NAME AUDIT FLAGGED, NOT DONE HERE: the existing 'claude' row's
-- display_name is 'Claude.ai' -- a URL-shaped name, inconsistent with
-- 'ChatGPT'/'Gemini'/'Mistral'. Jason flagged this as needing a full
-- audit of every existing display_name for user-facing consistency, plus
-- an audit that all UI/user-facing messages render display_name and never
-- id -- tracked separately (see items table), not fixed in this
-- migration to keep this change scoped to what was actually decided.
--
-- SOURCING, per decisions.id=684's documentation-gate standard (citable,
-- first-party sources only):
-- - Groq Chat (chat.groq.com): Groq's single Privacy Policy explicitly
--   covers "Groq chat" alongside its website/APIs/services (groq.com/
--   privacy-policy, and a still-live 2024 PDF version confirming the
--   same scope) -- no separate consumer-web policy exists, so this row
--   reuses the existing groq row's already-curated documentation_gate
--   findings (contractual DPA, no training by default, 180-day deletion
--   commitment post-termination) rather than requiring a wholly separate
--   review.
-- - Mistral Vibe (chat.mistral.ai): confirmed via Mistral's own current
--   docs (docs.mistral.ai/admin/security-access/privacy, retrieved
--   2026-09-12) -- Vibe Free tier conversations may be used to improve
--   models by default, opt-out available via Admin Panel; Vibe Pro/Team/
--   Enterprise are not used for training by default. No Zero Data
--   Retention option exists on Vibe at any tier (unlike the API, which
--   does offer ZDR on the Scale plan) -- a real posture gap from the
--   existing mistral row's API-only findings, reflected in this row's
--   own documentation_gate rather than copied from the API row.
-- - DeepSeek (chat.deepseek.com): full first-party findings already
--   completed and recorded in items.id=347 (Chat-PM, 2026-09-12) --
--   PRC data controller/storage/dispute jurisdiction, indefinite
--   account-lifetime retention, default training-use with in-product
--   opt-out, phone/email-verified account required. Reused verbatim here.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE providers ADD COLUMN preference_tier TEXT NOT NULL DEFAULT 'supported';

UPDATE providers SET preference_tier = 'preferred'
WHERE id IN ('duckai', 'claude', 'chatgpt', 'gemini');

INSERT INTO providers
    (id, display_name, provider_type, mode, launch_url, activation_status,
     documentation_gate, last_reviewed_at, review_trigger_note, created_at,
     is_local, login_required, is_anonymous, retains_data,
     trains_on_data_by_default, privacy_guardian_default_level, risk_rating,
     user_privacy_summary, preference_tier)
VALUES
    ('groqchat', 'Groq Chat', 'external_service', 'embedded_web',
     'https://chat.groq.com/', 'active',
     '{"pending_curation":false,"summary":"Groq''s single Privacy Policy covers Groq chat (chat.groq.com) alongside its website, APIs, and services -- no separate consumer-web policy exists. Groq''s Services Agreement and Data Processing Addendum state that customer prompts and completions are not used to train Groq''s models by default, and are retained only as long as needed to provide the service -- the DPA commits to deleting customer data within a maximum of 180 days after contract termination.","jurisdiction":"United States (Groq, Inc.) -- per Groq''s Services Agreement, reviewed items.id=440.","sources":["https://groq.com/privacy-policy","https://console.groq.com/docs/legal"],"scope_note":"Reuses the existing groq (Tier 1.5 API) row''s items.id=440 curation findings -- confirmed this session that Groq''s Privacy Policy explicitly scopes to cover Groq chat, not just the API."}',
     datetime('now'),
     'Curated 2026-09-12, reusing items.id=440''s Groq review (confirmed single privacy policy covers chat.groq.com).',
     datetime('now'),
     0, 1, 0, 0, 0, 'low', 1,
     '{"summary": "Groq doesn''t use your prompts or responses to train its models by default, and the same privacy policy that covers Groq''s API also covers Groq Chat.", "sources_url": "https://groq.com/privacy-policy"}',
     'supported'),

    ('mistralvibe', 'Mistral Vibe', 'external_service', 'embedded_web',
     'https://chat.mistral.ai/', 'active',
     '{"pending_curation":false,"summary":"Mistral Vibe (formerly Le Chat) Free tier: conversations may be used to improve Mistral''s models by default, opt-out available via the Admin Panel. Vibe Pro/Team/Enterprise: conversations are not used for model training by default. No Zero Data Retention option exists on Vibe at any tier -- a real gap from Mistral''s API product, which does offer ZDR on its Scale plan. Chat retention is admin-configurable (Never, 30, 60, 90, 180 days, or 1 year) for paid tiers.","jurisdiction":"European Union (Mistral AI, France) -- GDPR-governed by design, per CLAUDE.md''s Tier 1.5 tenet.","sources":["https://docs.mistral.ai/admin/security-access/privacy","https://legal.mistral.ai/terms/privacy-policy"],"scope_note":"This is Vibe (the consumer chat product), assessed separately from the existing mistral (Tier 1.5 API) row -- the two have materially different training-use defaults. Free-tier-web trains by default, the paid API does not."}',
     datetime('now'),
     'Curated 2026-09-12, first-party review of docs.mistral.ai privacy/data-controls documentation.',
     datetime('now'),
     0, 1, 0, 1, 1, 'medium', 2,
     '{"summary": "Mistral Vibe''s free tier can use your conversations to improve its models unless you opt out. Paid tiers do not train by default, but no Zero Data Retention option exists at any tier.", "settings_url": "https://docs.mistral.ai/admin/security-access/privacy"}',
     'supported'),

    ('deepseek', 'DeepSeek', 'external_service', 'embedded_web',
     'https://chat.deepseek.com/', 'active',
     '{"pending_curation":false,"summary":"DeepSeek''s official Privacy Policy: data controller is Hangzhou DeepSeek Artificial Intelligence Co., Ltd. Data is directly collected, processed, and stored in the People''s Republic of China. Retention is tied to account lifetime for account/input/payment data, no fixed deletion timeline disclosed. Inputs/outputs used to train/improve models by default, opt-out available via an in-product toggle. Terms of Service separately make PRC law the governing law for the agreement itself, with dispute jurisdiction in Chinese courts where DeepSeek is registered.","jurisdiction":"People''s Republic of China (Hangzhou DeepSeek Artificial Intelligence Co., Ltd.) -- data storage AND contractual dispute jurisdiction, per DeepSeek''s own Privacy Policy and Terms of Service.","contradictory_reporting":"None found -- DeepSeek''s own policy is unusually direct/undisguised about PRC jurisdiction and default training use, not evasive.","sources":["https://cdn.deepseek.com/policies/en-US/deepseek-privacy-policy.html","https://cdn.deepseek.com/policies/en-US/deepseek-terms-of-use.html"],"scope_note":"Full assessment: items.id=347 (Chat-PM, 2026-09-12). Passes decisions.id=710''s documentation-gate addition criteria (legible, documented ToS/retention policy, disclosed jurisdiction, no contradictory third-party reporting) but sits meaningfully weaker than every other Tier 3 provider on privacy substance -- Jason''s explicit decision to add as supported, not preferred."}',
     datetime('now'),
     'Curated 2026-09-12 per items.id=347''s completed privacy/feasibility assessment.',
     datetime('now'),
     0, 1, 0, 1, 1, 'high', 3,
     '{"summary": "DeepSeek stores your data in China and uses it to train its models by default (opt-out available). It requires an account, and both data storage and any legal disputes fall under Chinese jurisdiction.", "actions": [{"label": "Opt out of training", "description": "Disable the model-improvement toggle in DeepSeek''s account settings."}], "settings_url": "https://cdn.deepseek.com/policies/en-US/deepseek-privacy-policy.html"}',
     'supported');

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (18, datetime('now'),
    'items.id=347/486-adjacent: providers.preference_tier column (preferred/supported). duckai/claude/chatgpt/gemini marked preferred. Three new Tier 3 external_service rows (groqchat, mistralvibe, deepseek) for web-chat access, distinct from groq/mistral''s existing Tier 1.5 API rows -- full sourcing/rationale in this file''s own header comments');
