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
    hardware_requirement            TEXT
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
    'items.id=427/428: providers table (generalizes tier3_providers -- flag-based, no tier column, 4 existing rows migrated with derived flags) + user_provider_preference table (Focus>Persona>account-wide cascading provider configuration)');
