-- shared_013.sql
--
-- items.id=427/428 (PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md Parts 2/2b) --
-- generalizes tier3_providers into a flag-based providers table spanning
-- every tier (1, 1.5, 2, 3, and any future tier), and adds
-- user_provider_preference for per-user/Persona/Focus provider
-- configuration. Core design rule (Part 1): tier is a display label only,
-- never stored, never branched on by code -- this migration removes the
-- tier column outright, with no replacement column. Every eligibility/
-- routing decision going forward reads a decided flag column instead.
--
-- WHY A NEW VERSIONED FILE, NOT AN IN-PLACE shared_001.sql EDIT: unlike
-- this project's "pre-release, zero shipped users" consolidation precedent
-- (used for shared_001.sql itself), tier3_providers is a live table with
-- real seeded rows (documentation_gate research, risk_rating) and two live
-- consumers (provider_store::list_active_providers, Privacy Guardian's
-- max_risk_rating_for_providers) -- exactly why risk_rating itself landed
-- as shared_012.sql rather than an in-place edit. Same precedent applies
-- here, one level up.
--
-- MIGRATION SHAPE: create providers, copy tier3_providers' 4 rows across
-- (mapping old columns 1:1 where they carry forward, deriving the new
-- decided-flag columns from each row's own already-compiled
-- documentation_gate research -- see per-row comments below), then drop
-- tier3_providers. Per the spec: "generalizes tier3_providers ... does not
-- sit beside it as a second source of truth" -- no other code reads
-- tier3_providers after provider_store.rs is repointed at providers in the
-- same change, so nothing is left dangling.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

-- providers (Part 2): global, flag-based, no tier column. Column-by-column
-- rationale below only covers what's NEW or CHANGED vs. tier3_providers --
-- see shared_001.sql's own tier3_providers header for the rationale behind
-- columns carried forward unchanged (documentation_gate, activation_status,
-- launch_url, created_at, last_reviewed_at, review_trigger_note).
--
--   provider_type: open vocabulary, no CHECK -- mechanical integration
--     shape ('local_model' | 'cloud_inference_api' | 'split_screen_web' |
--     'external_service'), not a classification -- matches keys_001.sql's
--     open key_type precedent, deliberately NOT tier's old closed 2-value
--     CHECK domain (the exact structural problem this migration removes).
--   mode: adds 'local' to the existing 'embedded_web'|'api' CHECK set.
--     DEFAULT 'embedded_web' dropped -- mode is a decided column now like
--     the others below, always passed explicitly at insert
--     (provider_store::create_provider takes it as a required field).
--   is_local / is_anonymous / retains_data / trains_on_data_by_default /
--     qr_internal_eligible / privacy_guardian_default_level: new decided
--     flag columns (Part 2). Human-reviewed booleans/enum, set at curation
--     time, never derived from other columns or parsed out of
--     documentation_gate's freeform prose (core rule 2) -- risk_rating
--     already established this pattern, these follow it.
--   qr_internal_eligible DEFAULT 0: Part 3b's floor mechanism -- a provider
--     must be explicitly marked eligible; unmarked is exclusion, not an
--     open question. No migrated row is marked eligible (none of the 4
--     migrated providers are QR-internal-operation candidates).
--   privacy_guardian_default_level: TEXT, deliberately parallel to
--     ReviewTier's own three values (conductor/privacy/types.rs) exactly
--     as shared_012.sql's risk_rating comment already established for that
--     column -- same reasoning, extended to this one.
--   hardware_requirement: TEXT JSON, nullable. Tier 1/1.5 rows only (none
--     exist yet -- Ollama/local provider rows are greenfield, out of scope
--     for this migration). Objectively measurable data (min RAM/VRAM
--     class, reference tokens/sec), unlike the flags above, so JSON is an
--     acceptable escape hatch here the same way it is for
--     documentation_gate/extra_metadata.
--   risk_rating: carried unchanged from shared_012.sql -- same column,
--     same CHECK, same DEFAULT 3 fail-safe, same live Privacy Guardian
--     consumer.
--   user_privacy_summary: TEXT JSON, nullable, no CHECK -- mirrors
--     documentation_gate's JSON-in-TEXT convention exactly (same escape-
--     hatch rationale). Holds the consumer-facing plain-language privacy
--     summary (this row's own "what this means for you" explainer content)
--     shown directly to the user -- distinct from documentation_gate's
--     compliance/audit-trail research framing. NULL until curated; no
--     DEFAULT '{}' since "not yet summarized" is a meaningfully different
--     state from "reviewed, no summary" for this display-facing field.
CREATE TABLE IF NOT EXISTS providers (
    id                              TEXT PRIMARY KEY,
    display_name                    TEXT NOT NULL,
    provider_type                   TEXT NOT NULL,
    mode                            TEXT NOT NULL
                                        CHECK (mode IN ('embedded_web', 'api', 'local')),
    launch_url                      TEXT,
    activation_status               TEXT NOT NULL DEFAULT 'active'
                                        CHECK (activation_status IN ('active', 'deprecated')),
    documentation_gate              TEXT NOT NULL DEFAULT '{}',
    last_reviewed_at                TEXT,
    review_trigger_note             TEXT,
    created_at                      TEXT NOT NULL,
    is_local                        INTEGER NOT NULL DEFAULT 0
                                        CHECK (is_local IN (0, 1)),
    is_anonymous                    INTEGER NOT NULL DEFAULT 0
                                        CHECK (is_anonymous IN (0, 1)),
    retains_data                    INTEGER NOT NULL DEFAULT 1
                                        CHECK (retains_data IN (0, 1)),
    trains_on_data_by_default       INTEGER NOT NULL DEFAULT 1
                                        CHECK (trains_on_data_by_default IN (0, 1)),
    login_required                  INTEGER NOT NULL
                                        CHECK (login_required IN (0, 1)),
    qr_internal_eligible            INTEGER NOT NULL DEFAULT 0
                                        CHECK (qr_internal_eligible IN (0, 1)),
    privacy_guardian_default_level  TEXT
                                        CHECK (privacy_guardian_default_level IS NULL
                                            OR privacy_guardian_default_level IN ('low', 'medium', 'high')),
    risk_rating                     INTEGER NOT NULL DEFAULT 3
                                        CHECK (risk_rating IN (1, 2, 3)),
    hardware_requirement            TEXT,
    user_privacy_summary            TEXT
);

CREATE INDEX IF NOT EXISTS idx_providers_selector
    ON providers (activation_status, provider_type, login_required);

CREATE INDEX IF NOT EXISTS idx_providers_qr_internal
    ON providers (qr_internal_eligible)
    WHERE qr_internal_eligible = 1;

-- Data migration: carry tier3_providers' 4 real seeded rows across.
-- provider_type derived mechanically from the old tier value per Part 1's
-- own tier definitions (tier=2 rows are Tier 2's "split-screen, anonymous"
-- shape; tier=3 rows are Tier 3's "full external service" shape).
--
-- retains_data / trains_on_data_by_default / privacy_guardian_default_level
-- are read from each row's OWN documentation_gate JSON (already-compiled
-- research, not fresh research) -- this is a judgment call restated in full
-- in this session's handoff for Chat-PM/Jason to independently verify:
--   duckai:  retains_data=0 ("not retained by DuckDuckGo by default");
--            trains_on_data_by_default=0 (no training claim in the doc;
--            PII stripped before forwarding); default_level=low (mirrors
--            risk_rating=1).
--   claude:  retains_data=1 (30-day deletion window, up to 5yr when
--            training-enabled); trains_on_data_by_default=1 (Aug 2025
--            change: "opted IN to model-training use ... by default ...
--            users must actively opt out"); default_level=high (mirrors
--            risk_rating=3).
--   chatgpt: retains_data=1 (30-day window + documented NYT litigation-hold
--            history overriding it); trains_on_data_by_default=1 ("Free and
--            Plus tier conversations are used for model training by
--            default unless the user opts out"); default_level=high.
--   gemini:  retains_data=1 (18mo default + 3yr human-review carve-out that
--            survives user deletion); trains_on_data_by_default=1 (the
--            human-review carve-out is evidence of default-on
--            training/QA use, same spirit as the doc's own caution note);
--            default_level=high.
-- is_anonymous is mechanically NOT login_required for all 4 -- low-risk,
-- since login_required was already a decided column, not new judgment.
-- is_local=0 and hardware_requirement=NULL for all 4 (none are local).
INSERT INTO providers
    (id, display_name, provider_type, mode, launch_url, activation_status,
     documentation_gate, last_reviewed_at, review_trigger_note, created_at,
     is_local, is_anonymous, retains_data, trains_on_data_by_default,
     login_required, qr_internal_eligible, privacy_guardian_default_level,
     risk_rating, hardware_requirement)
SELECT
    id,
    display_name,
    CASE WHEN tier = 2 THEN 'split_screen_web' ELSE 'external_service' END,
    mode,
    launch_url,
    activation_status,
    documentation_gate,
    last_reviewed_at,
    review_trigger_note,
    created_at,
    0,
    CASE WHEN login_required = 0 THEN 1 ELSE 0 END,
    CASE WHEN id = 'duckai' THEN 0 ELSE 1 END,
    CASE WHEN id = 'duckai' THEN 0 ELSE 1 END,
    login_required,
    0,
    CASE WHEN risk_rating = 1 THEN 'low' WHEN risk_rating = 2 THEN 'medium' ELSE 'high' END,
    risk_rating,
    NULL
FROM tier3_providers;

DROP TABLE tier3_providers;

-- Post-migration curation pass (this session, 2026-09-07): documentation_gate
-- content amendments for claude and gemini (one new finding each -- see
-- per-row comments below) plus initial user_privacy_summary values for all
-- 4 migrated providers. documentation_gate is carried unchanged from
-- tier3_providers by the INSERT...SELECT above (plain column reference, no
-- CASE derivation) -- these UPDATEs are the only way to amend that content
-- without touching shared_001.sql, consistent with editing this migration
-- in place rather than adding a new one (both shared_013.sql and
-- shared_014.sql confirmed unpushed this session). Full JSON literals, not
-- json_patch/json_set -- no migration in this codebase uses SQL JSON1
-- functions, and documentation_gate is otherwise always written as a
-- complete literal blob (shared_001.sql, shared_014.sql); matching that
-- convention rather than introducing a new one.
--
-- chatgpt is NOT amended beyond user_privacy_summary: its existing
-- contradictory_reporting field (Jan 5 2026 discovery order, July 2026
-- misrepresentation allegation, specific court citations) is already
-- accurate and more complete than an earlier-considered replacement would
-- have been -- verified by reading shared_001.sql's seed directly. duckai
-- is likewise NOT amended -- its documentation_gate was already accurate.

-- claude: adds the Anthropic Usage Policy retention carveout -- content
-- flagged for a Usage Policy violation is retained on a separate, longer
-- schedule than the training opt-out setting otherwise controls. Not
-- previously captured in the July 2026 review.
UPDATE providers SET
    documentation_gate = '{"tos_url":"https://privacy.claude.com/en/articles/9301722-updates-to-our-acceptable-use-policy-now-usage-policy-consumer-terms-of-service-and-privacy-policy","retention_summary":"Default backend retention is 30 days for deleted conversations -- removed from chat history immediately on deletion, purged from Anthropic backend systems within 30 days. Consumer accounts (Free, Pro, Max) are opted IN to model-training use of conversations by default as of the August 2025 policy change -- users must actively opt out. Training-enabled data is retained for up to 5 years. Deleting a conversation excludes it from future training.","jurisdiction":"Anthropic PBC, USA, for US and rest-of-world consumer accounts. Anthropic Ireland, Limited is the data controller and consumer-terms counterparty for EEA, UK, and Swiss users.","contradictory_reporting":"None found this review.","review_caveat":"August 2025 policy change flipped the consumer training default from opt-in to opt-out-required -- a material posture shift from earlier project evaluations of this provider. Re-verify if Anthropic changes the default again.","sources":["https://privacy.claude.com/en/articles/9301722-updates-to-our-acceptable-use-policy-now-usage-policy-consumer-terms-of-service-and-privacy-policy","https://www.anthropic.com/news/updates-to-our-consumer-terms"],"usage_policy_carveout":"Content flagged as violating Anthropic''s Usage Policy may be retained (2 years for inputs/outputs, 7 years for classification scores) regardless of the user''s training opt-out setting, per Anthropic''s July 2026 Privacy Center update."}',
    user_privacy_summary = '{"summary": "Claude Free, Pro, and Max plans train on your conversations by default since August 2025. Turning this off in Settings restores the original 30-day deletion window instead of 5-year retention.", "account_status_note": "Claude for Work, Enterprise, Education, and API usage are never used for training, regardless of this setting.", "actions": [{"label": "Turn off model training", "description": "Settings -> Privacy -> disable ''Improve Claude for everyone.''"}, {"label": "Use Incognito chats", "description": "One-off conversations that are never used for training, even with the main toggle on."}], "settings_url": "https://claude.ai/settings/data-privacy-controls"}'
WHERE id = 'claude';

-- gemini: adds a scope note -- this row's assessment is about Gemini AI
-- Chat specifically, not other Google products with their own, separate
-- privacy posture changes (e.g. Google Search's June 2026 saved-media
-- change), which a reader could otherwise conflate with this card.
UPDATE providers SET
    documentation_gate = '{"tos_url":"https://support.google.com/gemini/answer/13594961","retention_summary":"Default retention is 18 months for Gemini Apps Activity, user-configurable to 3 or 36 months or indefinite. Keep Activity off reduces retention to 72 hours for most conversations. A subset of conversations selected for human review, for quality and safety purposes, is retained separately for up to 3 years and is NOT deleted when the user deletes their activity -- this human-reviewed subset does not follow the headline retention window.","jurisdiction":"Google Ireland Limited for EEA and Switzerland users. Google LLC, USA, for all other users.","contradictory_reporting":"No third-party contradiction found this review, but flagging an internal-policy caveat worth surfacing on the card: the up-to-3-years human-review carve-out, which survives user deletion, materially changes the retention story beyond the headline 18-month figure -- same spirit as the Groq precedent of not taking the top-line retention number at face value.","sources":["https://support.google.com/gemini/answer/13594961"],"scope_note":"This assessment covers Gemini AI Chat specifically, not other Google products (e.g. a separate June 2026 change affects Google Search''s use of saved media for AI training)."}',
    user_privacy_summary = '{"summary": "Gemini keeps your conversations for 18 months by default and may use them to improve Google''s AI. A sample may be reviewed by a human, and reviewed conversations are kept for up to 3 years even if you delete your activity.", "account_status_note": "Gemini accessed through a paid Google Workspace business account is not used for training and follows your organization''s data rules instead.", "actions": [{"label": "Turn off Gemini Apps Activity", "description": "myaccount.google.com -> Data & Privacy -> stops future conversations from being saved or used for training."}, {"label": "Use Temporary Chat", "description": "Conversations that disappear after the session and aren''t used for training."}], "settings_url": "https://myactivity.google.com/product/gemini"}'
WHERE id = 'gemini';

-- chatgpt: user_privacy_summary only -- documentation_gate intentionally
-- untouched (see comment above this block).
UPDATE providers SET
    user_privacy_summary = '{"summary": "ChatGPT trains on your conversations by default on Free and Plus plans unless you turn it off. Temporary Chat mode keeps a conversation out of your history and out of training entirely.", "account_status_note": "ChatGPT Team, Enterprise, and Edu accounts are excluded from training by default.", "actions": [{"label": "Turn off model training", "description": "Settings -> Data Controls -> disable ''Improve the model for everyone.''"}, {"label": "Use Temporary Chat", "description": "Conversations that don''t save to history or train the model."}], "settings_url": "https://help.openai.com/en/articles/7730893-data-controls-faq"}'
WHERE id = 'chatgpt';

-- duckai: user_privacy_summary only -- documentation_gate intentionally
-- untouched (already accurate).
UPDATE providers SET
    user_privacy_summary = '{"summary": "Duck.ai proxies your request so Anthropic, OpenAI, and other model providers never see your identity, and doesn''t use your chats to train any model. Chats save locally on your device by default, not on DuckDuckGo''s servers.", "account_status_note": "No account needed. An optional ''Sync & Backup'' feature can store encrypted chats on DuckDuckGo''s servers, but only your device holds the decryption key.", "actions": [{"label": "Use the Fire Button", "description": "Instantly clears all local chat history from Duck.ai."}], "settings_url": "https://duckduckgo.com/duckai/privacy-terms"}'
WHERE id = 'duckai';

-- user_provider_preference (Part 2b): per-user configuration, scoped
-- User x Persona x Focus. Cannot live on providers (global table would
-- corrupt across users on this multi-user install). Generalizes
-- integration_keys_store's per-user-per-provider credential-presence
-- pattern and get_capability_profile's live-check-never-cached convention
-- (last_verified_at here mirrors that, not a trusted cache).
--
-- SCOPE, not Group: confirmed live this session (per the spec) that
-- group.db has no settings-cascade mechanism -- it's Persona-scoped
-- document sharing, not User membership with cascading settings. A shared
-- Group Focus requirement collapses into an ordinary Focus-level row.
--
-- STORAGE SHAPE: the account-wide row (persona_id IS NULL AND focus_id IS
-- NULL) is the comprehensive record. A Persona- or Focus-scoped row is a
-- stub that exists only when that scope needs to diverge from what it
-- would otherwise inherit -- most Personas/Focuses have no override rows
-- at all for most providers.
--
-- NEW-USER DEFAULT: matches user_capabilities's own documented convention
-- exactly -- absence of any row (at any scope) means a system-wide
-- hardcoded default applies, never inheritance from another user's rows.
-- Verified live this session: auth/user_store.rs::create_user inserts only
-- users, user_salts, user_sharing_keys -- no copying from any other user's
-- configuration happens anywhere in account creation today.
--
-- UNIQUENESS: SQLite's composite-PK NULL-distinctness means a PK alone
-- can't stop two account-wide (persona_id/focus_id both NULL) rows for the
-- same (user_id, provider_id). user_capabilities closed this same gap with
-- a partial unique index for its one NULL-scoped level; this table extends
-- that same approach across all three scope levels below, rather than
-- integration_keys's alternative (plain UNIQUE + app-level
-- check-then-upsert) -- the spec explicitly names user_capabilities as the
-- pattern to mirror, so its own DB-level-enforcement approach is followed
-- here.
--
-- subscription_status: Part 4c's recommended dedicated column (paid vs.
-- free tier per provider) -- cheap to capture now, potentially relevant to
-- items.id=151's future direct-API scoping even though nothing in this
-- design currently branches on it. Nullable: most rows won't set it.
CREATE TABLE IF NOT EXISTS user_provider_preference (
    id                    TEXT PRIMARY KEY,
    user_id               TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    persona_id            TEXT REFERENCES personas(id) ON DELETE CASCADE,
    focus_id              TEXT,
    provider_id           TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    login_available       INTEGER NOT NULL DEFAULT 0
                              CHECK (login_available IN (0, 1)),
    user_preference       TEXT NOT NULL
                              CHECK (user_preference IN ('preferred', 'allowed', 'declined')),
    enabled_at            TEXT,
    declined_at           TEXT,
    local_model_version   TEXT,
    installed_at          TEXT,
    last_verified_at      TEXT,
    subscription_status   TEXT
                              CHECK (subscription_status IS NULL
                                  OR subscription_status IN ('free', 'paid')),
    created_at            TEXT NOT NULL
);

-- Account-wide row uniqueness (persona_id and focus_id both NULL).
CREATE UNIQUE INDEX IF NOT EXISTS idx_user_provider_pref_account
    ON user_provider_preference (user_id, provider_id)
    WHERE persona_id IS NULL AND focus_id IS NULL;

-- Persona-wide row uniqueness (persona_id set, focus_id NULL).
CREATE UNIQUE INDEX IF NOT EXISTS idx_user_provider_pref_persona
    ON user_provider_preference (user_id, persona_id, provider_id)
    WHERE persona_id IS NOT NULL AND focus_id IS NULL;

-- Focus-specific row uniqueness (focus_id set -- already pins a single
-- Persona, so provider_id + focus_id + user_id is the natural key here).
CREATE UNIQUE INDEX IF NOT EXISTS idx_user_provider_pref_focus
    ON user_provider_preference (user_id, focus_id, provider_id)
    WHERE focus_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_user_provider_pref_lookup
    ON user_provider_preference (user_id, provider_id);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (13, datetime('now'),
    'items.id=427/428: providers table (generalizes tier3_providers -- flag-based, no tier column, 4 existing rows migrated with derived flags) + user_provider_preference table (Focus>Persona>account-wide cascading provider configuration) + user_privacy_summary column (consumer-facing privacy explainer, distinct from documentation_gate) + a curation pass amending claude/gemini documentation_gate and setting user_privacy_summary for all 4 migrated rows');
