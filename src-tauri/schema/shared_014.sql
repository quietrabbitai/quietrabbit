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
-- I am not the curator (spec core rule 3: flags are decided at curation
-- time by a human). Only mechanically-known facts are set explicitly below
-- (is_local, login_required, is_anonymous, provider_type, mode) plus
-- documentation_gate content sourced from CLAUDE.md's own existing "Tier
-- 1.5" tenet (Groq=US/free tier, Mistral=EU-GDPR/paid) -- not fresh ToS/
-- retention research, and explicitly marked pending_curation in the JSON
-- itself rather than presented as verified. Every column requiring real
-- policy research (retains_data, trains_on_data_by_default, risk_rating,
-- qr_internal_eligible, privacy_guardian_default_level) is left at the
-- table's own conservative DEFAULTs (1/1/3/0/NULL) by omission from this
-- INSERT's column list, not fabricated. review_trigger_note flags these as
-- provisional; a one-time full audit of all 6 providers rows plus a
-- recurring maintenance-check cadence is scoped as its own future item
-- (items.id=440, blocked on this migration landing) -- not designed here.
INSERT OR IGNORE INTO providers
    (id, display_name, provider_type, mode, launch_url, activation_status,
     documentation_gate, last_reviewed_at, review_trigger_note, created_at,
     is_local, login_required, is_anonymous)
VALUES
    ('groq', 'Groq', 'cloud_inference_api', 'api', NULL, 'active',
     '{"pending_curation":true,"summary":"Fast hosted inference, same capability class as local Tier 1 (small/open-source, not frontier) -- exists purely to be faster than the user''s own hardware. Non-anonymous (signup required). US-based, free tier at time of writing (CLAUDE.md Tier 1.5 tenet).","jurisdiction":"United States (per CLAUDE.md; not independently re-verified this session).","note":"Row seeded items.id=429/430/432 to unblock Tier 1.5 provider selection -- retains_data/trains_on_data_by_default/risk_rating/qr_internal_eligible/privacy_guardian_default_level intentionally left at conservative schema defaults, not researched. See items.id=440 for the real curation audit."}',
     NULL,
     'Provisional row, items.id=429/430/432, 2026-09-06 -- unblocks Tier 1.5 provider selection (providers FK on user_provider_preference). Real documentation-gate curation deferred to items.id=440; do not treat retains_data/trains_on_data_by_default/risk_rating as reviewed facts yet.',
     datetime('now'),
     0, 1, 0),

    ('mistral', 'Mistral', 'cloud_inference_api', 'api', NULL, 'active',
     '{"pending_curation":true,"summary":"Fast hosted inference, same capability class as local Tier 1 (small/open-source, not frontier) -- exists purely to be faster than the user''s own hardware. Non-anonymous (signup required). EU/GDPR jurisdiction, paid at time of writing (CLAUDE.md Tier 1.5 tenet).","jurisdiction":"European Union / GDPR (per CLAUDE.md; not independently re-verified this session).","note":"Row seeded items.id=429/430/432 to unblock Tier 1.5 provider selection -- retains_data/trains_on_data_by_default/risk_rating/qr_internal_eligible/privacy_guardian_default_level intentionally left at conservative schema defaults, not researched. See items.id=440 for the real curation audit."}',
     NULL,
     'Provisional row, items.id=429/430/432, 2026-09-06 -- unblocks Tier 1.5 provider selection (providers FK on user_provider_preference). Real documentation-gate curation deferred to items.id=440; do not treat retains_data/trains_on_data_by_default/risk_rating as reviewed facts yet.',
     datetime('now'),
     0, 1, 0);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (14, datetime('now'),
    'items.id=429/430/432: focus_provider_criteria table (Part 3c, additive/parallel -- not yet wired into enforcement) + groq/mistral seeded as providers rows (cloud_inference_api) to unblock Tier 1.5 provider selection');
