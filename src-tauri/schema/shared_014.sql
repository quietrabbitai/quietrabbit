-- shared_014.sql
--
-- items.id=429/430/432 (PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md Part 3c) --
-- adds focus_provider_criteria (the Focus-level provider ceiling/allowlist
-- record) and seeds groq/mistral into providers (needed by items.id=430/432
-- to identify and select the Tier 1.5 provider set, and required by
-- user_provider_preference's FK on providers.id -- neither row existed
-- before this migration; see shared_001.sql's own tier3_providers seed
-- comment for why they were deliberately excluded from that table).
--
-- SCOPE, deliberate (judgment call 1, this session's plan): this migration
-- is ADDITIVE, parallel infrastructure only. It does NOT touch
-- focus_settings.max_permitted_tier/privacy_tier, StepDefinition.routing_tier,
-- or any of gate3.rs/failure.rs/lifecycle.rs's numeric-tier ceiling
-- enforcement -- those are golden-vector-verified Privacy Guardian code
-- (CLAUDE.md) and a real cutover has to additionally design a flag-based
-- replacement for privacy_tier's abstraction-tier role and routing_tier's
-- step-authoring role, neither designed anywhere yet. A full cutover is a
-- separate future item.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

-- focus_provider_criteria (Part 3c): one row per Focus. require_* flags are
-- AND-composed -- "a provider must satisfy every set-true requirement"
-- (spec's own words) -- so a policy needing OR-across-flags semantics needs
-- multiple flags set true together on the SAME row (see this session's
-- judgment call 2: local_and_anonymous sets both require_is_local AND
-- require_is_anonymous, not a single-flag shortcut, so a future
-- non-anonymous local-model row correctly fails that policy while still
-- passing local_only).
--
-- allow_provider_ids / deny_provider_ids: JSON arrays of provider ids.
-- Precedence at query time (application-layer, in
-- focus_provider_criteria_store::eligible_providers_for_focus): deny-list
-- excludes unconditionally -> allow-list includes unconditionally ->
-- remaining providers filtered by require_* flags.
--
-- seeded_from_policy: which named policy (local_only | local_and_anonymous |
-- no_training_default | unrestricted) originally populated this row.
-- Display/reset only -- NEVER read by enforcement (the focus_profile
-- drift, items.id=230, is the explicit cautionary precedent this design
-- must not repeat: an earlier version of that code's own comment conflated
-- a shorthand label with independent enforcement logic).
--
-- focus_id is not an FK -- no `focuses` table exists anywhere in this
-- codebase; user_provider_preference.focus_id (shared_013.sql) already set
-- this precedent.
--
-- NOT WIRED INTO ENFORCEMENT YET (deliberate, judgment call 1): no code
-- reads this table this session. focus_settings.max_permitted_tier/
-- privacy_tier remain the live, authoritative ceiling.
CREATE TABLE IF NOT EXISTS focus_provider_criteria (
    focus_id                    TEXT PRIMARY KEY,
    require_is_local            INTEGER NOT NULL DEFAULT 0
                                    CHECK (require_is_local IN (0, 1)),
    require_is_anonymous        INTEGER NOT NULL DEFAULT 0
                                    CHECK (require_is_anonymous IN (0, 1)),
    require_not_trains_on_data  INTEGER NOT NULL DEFAULT 0
                                    CHECK (require_not_trains_on_data IN (0, 1)),
    allow_provider_ids          TEXT NOT NULL DEFAULT '[]',
    deny_provider_ids           TEXT NOT NULL DEFAULT '[]',
    seeded_from_policy          TEXT
                                    CHECK (seeded_from_policy IS NULL
                                        OR seeded_from_policy IN (
                                            'local_only', 'local_and_anonymous',
                                            'no_training_default', 'unrestricted'
                                        )),
    created_at                  TEXT NOT NULL,
    updated_at                  TEXT NOT NULL
);

-- Seed groq/mistral as providers rows (items.id=430/432 prerequisite -- see
-- this migration's header). provider_type='cloud_inference_api' is the
-- flag-based way items.id=430/432 identify "the Tier 1.5 set" (a mechanical
-- integration-shape attribute, not a tier label -- spec core rule 2/3):
-- non-local, API-mode, login-required hosted inference, distinct from
-- Duck.ai's 'split_screen_web' and Claude/ChatGPT/Gemini's
-- 'external_service'.
--
-- CURATED IN PLACE, 2026-09-07 (items.id=440 Part A, this session): the
-- original seed below was provisional (pending_curation:true, only
-- mechanically-known facts set explicitly -- is_local, login_required,
-- is_anonymous, provider_type, mode -- with documentation_gate content
-- sourced from CLAUDE.md's "Tier 1.5" tenet rather than fresh ToS/DPA
-- research, and every column requiring real policy research left at the
-- table's conservative DEFAULTs (1/1/3/0/NULL) by omission from the column
-- list). Since shared_014.sql is an unpushed local commit (0e3d26c,
-- confirmed absent from origin/main), items.id=440 Part A's real research
-- is folded into this same INSERT rather than a follow-up migration --
-- the same "pre-release, zero shipped users" precedent shared_013.sql's own
-- header cites for itself. retains_data, trains_on_data_by_default,
-- privacy_guardian_default_level, and risk_rating are now explicit, sourced
-- values (see each row's own documentation_gate/review_trigger_note below),
-- not defaults-by-omission. qr_internal_eligible remains at its DEFAULT 0
-- (unmarked) -- QR-internal-operation eligibility was not part of this
-- audit's scope. A recurring maintenance-check cadence beyond this initial
-- curation is still not designed here.
INSERT OR IGNORE INTO providers
    (id, display_name, provider_type, mode, launch_url, activation_status,
     documentation_gate, last_reviewed_at, review_trigger_note, created_at,
     is_local, login_required, is_anonymous, retains_data,
     trains_on_data_by_default, privacy_guardian_default_level, risk_rating,
     user_privacy_summary)
VALUES
    ('groq', 'Groq', 'cloud_inference_api', 'api', NULL, 'active',
     '{"pending_curation":false,"summary":"Groq''s Services Agreement and Data Processing Addendum state that customer prompts and completions are not used to train Groq''s models by default, and are retained only as long as needed to provide the service. Groq''s DPA commits to deleting customer data within a maximum of 180 days after contract termination.","jurisdiction":"United States (Groq, Inc.) -- per Groq''s Services Agreement, reviewed items.id=440.","sources":["https://console.groq.com/docs/legal","https://groq.com/privacy-policy"]}',
     NULL,
     'Curated 2026-09-07, items.id=440 Part A audit -- primary sources: Groq Services Agreement and Data Processing Addendum, console.groq.com/docs/legal.',
     datetime('now'),
     0, 1, 0, 0, 0, 'low', 1,
     '{"summary": "Groq doesn''t use your prompts or responses to train its models, and its Cloud Services agreement says your data is retained only as long as needed to provide the service.", "account_status_note": "Applies to Groq''s API/Cloud Services; the general Groq website has separate, unrelated analytics/marketing data practices.", "actions": [{"label": "Request data deletion", "description": "Groq''s Data Processing Addendum guarantees deletion within 180 days of account termination."}], "settings_url": "https://groq.com/privacy-policy"}'),

    ('mistral', 'Mistral', 'cloud_inference_api', 'api', NULL, 'active',
     '{"pending_curation":false,"summary":"Mistral''s Data Processing Addendum (Section 2.3, effective July 27 2026) authorizes Mistral to use customer data for model training and improvement unless the customer is or has opted out -- training is opt-out, not automatically excluded, even under a paid API contract. Mistral''s free consumer tier (formerly ''Le Chat'', now ''Vibe'') trains on conversations by default. Mistral''s API keeps a 30-day rolling request log for abuse monitoring.","jurisdiction":"European Union (Mistral AI, France) -- GDPR-governed by design, per CLAUDE.md''s Tier 1.5 tenet.","sources":["https://legal.mistral.ai/terms/privacy-policy"]}',
     NULL,
     'Curated 2026-09-07, items.id=440 Part A audit -- primary sources: Mistral Data Processing Addendum Section 2.3 (effective 2026-07-27) and Mistral Privacy Policy, legal.mistral.ai/terms/privacy-policy.',
     datetime('now'),
     0, 1, 0, 1, 1, 'medium', 2,
     '{"summary": "Mistral is based in the EU and built under GDPR from the start, but training is opt-out rather than automatically excluded -- even paid API usage can be included unless you explicitly turn it off. The free consumer app trains on conversations by default.", "account_status_note": "Mistral''s paid API keeps request logs for 30 days for abuse monitoring; enabling Zero Data Retention (Scale plan) removes this.", "actions": [{"label": "Opt out of training", "description": "Admin Console -> Privacy -> disable ''Anonymous improvement data.''"}], "settings_url": "https://legal.mistral.ai/terms/privacy-policy"}');

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (14, datetime('now'),
    'items.id=429/430/432: focus_provider_criteria table (Part 3c, additive/parallel -- not yet wired into enforcement) + groq/mistral seeded as providers rows (cloud_inference_api) to unblock Tier 1.5 provider selection, curated in place (items.id=440 Part A) with real retains_data/trains_on_data_by_default/risk_rating/privacy_guardian_default_level/documentation_gate/user_privacy_summary values, not left at placeholder defaults');
