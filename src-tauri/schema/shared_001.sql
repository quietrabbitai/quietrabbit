-- persistence/schema/shared_001.sql
-- Instance database schema: shared.db
-- Stores personas, users, instance config, artifact versions, focus settings.
-- Not per-user encrypted — must be readable before any user logs in.
-- Contains no personal field values. instance_context limited to
-- general and personal sensitivity only (household name, shared prefs).
-- See ARCHITECTURE Section 8.1 for the explicit design rationale.
--
-- CONSOLIDATION NOTE (items.id=169, 2026-07-24): this file replaces the prior
-- five-migration chain: shared_001 (initial, 'spaces'/'space_id'), shared_002
-- (Phase A rename spaces->lives, D6-224/225), shared_003 (Phase B: topic_index +
-- asset_index stub), shared_004 (Phase C Persona migration lives->personas,
-- D6-289 through D6-303, + focus_settings table), shared_005 (seed
-- focus_settings for role-assessment Focus, all personas). Pre-release, zero
-- shipped users -- consolidated directly to final 'personas'/'persona_id'
-- naming, skipping the intermediate 'spaces'->'lives'->'personas' relay.
-- Design content preserved: focus_settings table + Persona-model rationale
-- (D6-291/294/297/299/302/303), both dev-fixture seed inserts (see their
-- own notes below -- these are NOT churn, they seed real dev bootstrap data).
-- Chat-DEV, per Chat-PM/Jason adjudication of Chat-DEV handoff id=99.
--
-- AUTH FOUNDATION CONSOLIDATION (items.id=205, 2026-08-01, Jason's direction):
-- users/user_salts edited directly to their final Architecture/
-- AUTH_MULTIUSER_ARCHITECTURE.md shape (role simplified, idle_timeout_minutes
-- added, Argon2id KDF params replace PBKDF2), and user_capabilities added,
-- rather than layered on as a separate shared_003.sql rebuild migration. Same
-- precedent as this file's own 2026-07-24 consolidation above: pre-release,
-- zero shipped users, no real install has ever built the pre-edit shape
-- (verified this session -- auth.rs::login/logout are fully stubbed, no Rust
-- code anywhere reads users.role's old CHECK values or
-- user_salts.kdf_algorithm/kdf_iterations), so there is no intermediate state
-- for a rebuild migration to walk through -- only a final shape to write.
-- shared_002.sql (items.id=92, focus_settings_friction_decisions) is a
-- genuinely separate, independently-shipped feature and is deliberately left
-- untouched by this consolidation -- not folded in, not evaluated for
-- collapsing here (flagged separately to Chat-PM as its own question, out of
-- items.id=205's scope).

CREATE TABLE IF NOT EXISTS schema_version (
    version         INTEGER PRIMARY KEY,
    applied_at      TEXT NOT NULL,
    description     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS migration_lock (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    locked_at       TEXT,
    locked_by       TEXT
);
INSERT OR IGNORE INTO migration_lock (id) VALUES (1);

-- Personas.
-- Persona model (D6-289, D6-291): personalization grouping only -- no tier
-- fields on the record. Tier enforcement is Focus-level (focus_settings),
-- not Persona-level. effective_tier = min(focus_def.max_routing_tier,
-- focus_settings.privacy_tier).
-- persona_type: open vocabulary, same value space as the retired life_type.
CREATE TABLE IF NOT EXISTS personas (
    id              TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    persona_type    TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

-- Users
-- role: simplified to user|admin (Architecture Section 7.1) -- the 'builder'
--   tier folded into persona-level functionality and no longer needs its
--   own role value.
-- idle_timeout_minutes: Section 3.2 -- per-user idle-lock timer, bounded
--   5-240, default 15. Not admin-lockable (no enforcement/override
--   mechanism exists anywhere in this schema or the codebase, consistent
--   with the doc's "not admin-lockable" framing).
CREATE TABLE IF NOT EXISTS users (
    id                          TEXT PRIMARY KEY,
    display_name                TEXT NOT NULL UNIQUE,
    role                        TEXT NOT NULL DEFAULT 'user'
                                    CHECK (role IN ('user', 'admin')),
    is_primary                  INTEGER NOT NULL DEFAULT 0,
    auth_enabled                INTEGER NOT NULL DEFAULT 0,
    password_hash               TEXT,
    tier2_provider_preference   TEXT
                                    CHECK (tier2_provider_preference IS NULL
                                        OR tier2_provider_preference IN ('mistral', 'groq')),
    idle_timeout_minutes        INTEGER NOT NULL DEFAULT 15
                                    CHECK (idle_timeout_minutes BETWEEN 5 AND 240),
    created_at                  TEXT NOT NULL,
    extra_metadata              TEXT NOT NULL DEFAULT '{}'
);

-- Enforce only one primary user per instance
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_single_primary
    ON users (is_primary) WHERE is_primary = 1;

-- User salts — includes KDF metadata for future algorithm upgrades.
-- Argon2id (Section 4.1): m=65536 KiB (64 MiB), t=3 iterations, p=4
-- parallelism lanes -- the "new application in 2026" starting profile the
-- architecture doc specifies. Stored per-account, not hardcoded, so a
-- future tuning change is a data change, not a schema change.
CREATE TABLE IF NOT EXISTS user_salts (
    user_id             TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    salt_hex            TEXT NOT NULL,
    kdf_algorithm       TEXT NOT NULL DEFAULT 'argon2id',
    kdf_memory_kib      INTEGER NOT NULL DEFAULT 65536,
    kdf_iterations      INTEGER NOT NULL DEFAULT 3,
    kdf_parallelism     INTEGER NOT NULL DEFAULT 4,
    created_at          TEXT NOT NULL
);

-- User-persona membership
CREATE TABLE IF NOT EXISTS user_personas (
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    persona_id  TEXT NOT NULL REFERENCES personas(id) ON DELETE CASCADE,
    joined_at   TEXT NOT NULL,
    PRIMARY KEY (user_id, persona_id)
);

-- Instance-level shared context (general and personal sensitivity ONLY)
CREATE TABLE IF NOT EXISTS instance_context (
    id              TEXT PRIMARY KEY,
    field_name      TEXT NOT NULL,
    field_value     TEXT NOT NULL,
    sensitivity     TEXT NOT NULL CHECK (sensitivity IN ('general', 'personal')),
    created_at      TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

-- Context groups (Release 1 schema, Release 2 UX)
CREATE TABLE IF NOT EXISTS context_groups (
    id              TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    persona_id      TEXT REFERENCES personas(id) ON DELETE CASCADE,
    created_at      TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS context_group_members (
    group_id    TEXT NOT NULL REFERENCES context_groups(id) ON DELETE CASCADE,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    joined_at   TEXT NOT NULL,
    PRIMARY KEY (group_id, user_id)
);

-- Artifact version tracking (spans all personas).
-- scope: 'persona' uses persona_id; '_global' for instance-wide artifacts.
-- artifact_type: 'guide'|'operator' (retired 'specialist' split, D6-224/225),
--   'focus' (retired 'path'). 'integration' intentionally excluded --
--   add when integrations are built.
CREATE TABLE IF NOT EXISTS artifact_versions (
    artifact_type   TEXT NOT NULL
                        CHECK (artifact_type IN ('guide', 'operator', 'focus')),
    artifact_id     TEXT NOT NULL,
    scope           TEXT NOT NULL DEFAULT '_global',
    persona_id      TEXT NOT NULL DEFAULT '_global',
    version         TEXT NOT NULL,
    trust_level     TEXT NOT NULL
                        CHECK (trust_level IN
                            ('official', 'reviewed', 'community', 'local_only')),
    revoked         INTEGER NOT NULL DEFAULT 0,
    installed_at    TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (artifact_type, artifact_id, scope, persona_id)
);

-- Instance configuration
CREATE TABLE IF NOT EXISTS instance_config (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);

INSERT OR IGNORE INTO instance_config VALUES ('role_enforcement', 'disabled');
INSERT OR IGNORE INTO instance_config VALUES ('auth_lockout_enabled', 'disabled');
INSERT OR IGNORE INTO instance_config VALUES ('instance_name', '');

-- Auth session tables (Release 1: schema present, NOT enforced)
CREATE TABLE IF NOT EXISTS auth_sessions (
    session_id      TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at      TEXT NOT NULL,
    last_active_at  TEXT NOT NULL,
    expires_at      TEXT NOT NULL,
    ip_address      TEXT,
    user_agent      TEXT,
    is_remember_me  INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_auth_sessions_user
    ON auth_sessions (user_id, expires_at);

CREATE TABLE IF NOT EXISTS auth_failures (
    id              TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    attempted_at    TEXT NOT NULL,
    ip_address      TEXT
);

CREATE INDEX IF NOT EXISTS idx_auth_failures_name
    ON auth_failures (display_name, attempted_at DESC);

CREATE TABLE IF NOT EXISTS auth_lockouts (
    display_name    TEXT PRIMARY KEY,
    locked_until    TEXT NOT NULL,
    failure_count   INTEGER NOT NULL DEFAULT 0,
    locked_at       TEXT NOT NULL
);

-- user_capabilities (Architecture Section 7.2) -- per-account capability
-- restrictions, e.g. a restricted child account that shouldn't be able to
-- create new personas or Focuses. Default-allow, deny-only rows: absence of
-- a row means allowed. persona_id nullable from day one; R1 only populates
-- NULL (account-wide) rows -- see the architecture doc for the full
-- most-specific-match-wins read-side semantics, not restated here.
CREATE TABLE IF NOT EXISTS user_capabilities (
    user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    persona_id   TEXT REFERENCES personas(id) ON DELETE CASCADE,  -- NULL = account-wide (R1)
    capability   TEXT NOT NULL,   -- open vocabulary: 'create_persona' | 'create_focus' | ...
    allowed      INTEGER NOT NULL DEFAULT 1,
    created_at   TEXT NOT NULL,
    PRIMARY KEY (user_id, persona_id, capability)
);

-- Enforce uniqueness for account-wide (persona_id IS NULL) capability rows.
-- SQLite's composite PRIMARY KEY treats each NULL as distinct, so the PK
-- above does not by itself prevent two account-wide rows for the same
-- (user_id, capability). This partial index closes that gap for the R1 case
-- (persona_id always NULL) without altering the PK the architecture doc
-- specifies verbatim -- once persona_id is a real value, the existing PK
-- already enforces uniqueness for that row on its own.
CREATE UNIQUE INDEX IF NOT EXISTS idx_user_capabilities_account_wide
    ON user_capabilities (user_id, capability) WHERE persona_id IS NULL;

-- topic_index — Persona-level pointer to active topics (Phase B, D6-226+).
-- Allows Persona dashboard to surface active/paused topics across all
-- focuses without opening per-persona encrypted databases.
-- topic_id: references topics.id in outputs.db (cross-db FK by value only —
--   SQLite cannot enforce cross-database FK constraints).
-- lifecycle_state: mirrored from outputs.db topics table for dashboard queries.
--   outputs.db topics table is the authoritative source of truth.
--   topic_index.lifecycle_state is a cache copy — updated by Phase 5A and
--   Reconciliation Boot Check. On conflict, outputs.db governs.
-- display_name: resolved from topics.name OR topics.placeholder_name.
--   Never derived from user input content directly.
-- content_summary: NULL in Release 1. Phase 5B standing summary cache (R2+).
-- session_count: mirrored from plan_state.db topic_header — cache copy.
CREATE TABLE IF NOT EXISTS topic_index (
    topic_id            TEXT PRIMARY KEY,
    persona_id          TEXT NOT NULL,
    focus_id            TEXT NOT NULL,
    display_name        TEXT NOT NULL,
    lifecycle_state     TEXT NOT NULL
                            CHECK (lifecycle_state IN (
                                'active', 'paused', 'awaiting',
                                'complete', 'closed'
                            )),
    last_active_at      TEXT NOT NULL,
    session_count       INTEGER NOT NULL DEFAULT 0,
    content_summary     TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_topic_index_persona
    ON topic_index (persona_id, lifecycle_state, last_active_at DESC);

CREATE INDEX IF NOT EXISTS idx_topic_index_focus
    ON topic_index (focus_id, lifecycle_state);

-- asset_index — schema stub only, no CRUD store or UI in Release 1.
-- Activates in Layer 8+.
-- asset_id: UUID primary key.
-- persona_id: Persona-level scope for cross-focus access (Option B promotion).
-- focus_id: NULL = Persona-level asset. Non-null = focus-scoped asset.
-- asset_type: 'static' (discrete named artifact) | 'structured' (schema + append).
-- backing_type: 'local' (R1) | 'imported' | 'connected' (R2+ only).
-- name_sensitivity: sensitivity preset of the asset name itself.
--   Names with medical/financial sensitivity obfuscated in public display —
--   forward to Chat-BRAND for UI specification.
-- content_ref: pointer to actual content location (path or DB ref). NULL in R1.
-- Asset index dual-write invariant (ADR-013 Section 2.4):
--   All mutations use atomic transaction locking shared.db index pointer
--   before content layer commits. Orphaned entries detected by Boot Check.
CREATE TABLE IF NOT EXISTS asset_index (
    asset_id            TEXT PRIMARY KEY,
    persona_id          TEXT NOT NULL,
    focus_id            TEXT,
    asset_type          TEXT NOT NULL
                            CHECK (asset_type IN ('static', 'structured')),
    backing_type        TEXT NOT NULL DEFAULT 'local'
                            CHECK (backing_type IN ('local', 'imported', 'connected')),
    name                TEXT NOT NULL,
    name_sensitivity    TEXT NOT NULL DEFAULT 'standard'
                            CHECK (name_sensitivity IN (
                                'standard', 'sensitive', 'private', 'locked'
                            )),
    content_ref         TEXT,
    created_at          TEXT NOT NULL,
    last_modified_at    TEXT NOT NULL,
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_asset_index_persona
    ON asset_index (persona_id, focus_id, last_modified_at DESC);

-- focus_settings — three independent Focus settings per D6-291:
--   context_flow:       bidirectional (default) | receive_only | isolated
--   library_visibility: shared (default) | persona_visible | persona_hidden
--   privacy_tier:       1=red | 2=yellow (default) | 3=green
-- max_permitted_tier: hard Tier ceiling for this Focus -- lives here (not on
--   the Persona record) per D6-297. Enforced at AUTHORIZE in
--   conductor/lifecycle.rs.
-- focus_profile: convenience label mapping the three settings (D6-294):
--   open       = bidirectional + shared          + yellow (default)
--   organized  = bidirectional + persona_visible + yellow
--   protected  = receive_only  + persona_hidden  + red
-- voice_override: Focus-level voice JSON, overrides Persona baseline (D6-302).
--   NULL = inherit Persona voice profile. Topic-level overrides deferred Phase D.
--
-- focus_settings placement in shared.db (D6-299): Privacy Guardian must read
-- Focus settings before opening encrypted per-user stores. Settings are
-- behavioral config, not personal data.
-- Conductor asserts this row exists at AUTHORIZE -- fails with clear error if
-- absent, not null-pointer downstream (D6-303). Real users configure
-- focus_settings during onboarding; the seed rows below are dev-only fixtures.
CREATE TABLE IF NOT EXISTS focus_settings (
    persona_id          TEXT NOT NULL REFERENCES personas(id) ON DELETE CASCADE,
    focus_id            TEXT NOT NULL,
    context_flow        TEXT NOT NULL DEFAULT 'bidirectional'
                            CHECK (context_flow IN (
                                'bidirectional', 'receive_only', 'isolated'
                            )),
    library_visibility  TEXT NOT NULL DEFAULT 'shared'
                            CHECK (library_visibility IN (
                                'shared', 'persona_visible', 'persona_hidden'
                            )),
    privacy_tier        INTEGER NOT NULL DEFAULT 2
                            CHECK (privacy_tier BETWEEN 1 AND 3),
    max_permitted_tier  INTEGER NOT NULL DEFAULT 2
                            CHECK (max_permitted_tier BETWEEN 1 AND 3),
    focus_profile       TEXT NOT NULL DEFAULT 'open'
                            CHECK (focus_profile IN (
                                'open', 'organized', 'protected'
                            )),
    voice_override      TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    PRIMARY KEY (persona_id, focus_id)
);

-- Primary read path: get_focus_settings(focus_id) in focus_settings_store.rs.
CREATE INDEX IF NOT EXISTS idx_focus_settings_focus_id
    ON focus_settings (focus_id);

-- Seed focus_settings defaults for the three original Focuses (D6-303).
-- All three seeded as Open profile: bidirectional, shared, yellow, max_permitted_tier=2.
-- Dev bootstrap fixture: seeds the single first-created persona only
-- (SELECT ... ORDER BY created_at LIMIT 1) -- correct ONLY because dev
-- environments have exactly one persona at bootstrap. Real users configure
-- focus_settings during onboarding. Do NOT copy this LIMIT 1 pattern for a
-- new Focus seed -- it would leave every persona after the first without a
-- focus_settings row, causing a hard AUTHORIZE failure (D6-303). See the
-- role-assessment seed immediately below for the correct all-personas pattern.
INSERT OR IGNORE INTO focus_settings
    (persona_id, focus_id, context_flow, library_visibility,
     privacy_tier, max_permitted_tier, focus_profile, voice_override,
     created_at, updated_at)
SELECT
    p.id, f.focus_id, 'bidirectional', 'shared', 2, 2, 'open', NULL,
    datetime('now'), datetime('now')
FROM (SELECT id FROM personas ORDER BY created_at LIMIT 1) p
CROSS JOIN (
    SELECT 'research-and-buy' AS focus_id UNION ALL
    SELECT 'quick-ask'                     UNION ALL
    SELECT 'writing-assistant'
) f;

-- Seed focus_settings for role-assessment Focus (D6-303) -- Organized profile:
-- bidirectional, persona_visible, yellow, max_permitted_tier=2 (no Tier 3 in
-- Release 1 Role Assessment). Seeded for ALL existing personas (correct
-- pattern for a new Focus -- see caution note on the seed above).
INSERT OR IGNORE INTO focus_settings
    (persona_id, focus_id, context_flow, library_visibility,
     privacy_tier, max_permitted_tier, focus_profile, voice_override,
     created_at, updated_at)
SELECT
    p.id, 'role-assessment', 'bidirectional', 'persona_visible', 2, 2, 'organized', NULL,
    datetime('now'), datetime('now')
FROM personas p;

-- tier3_providers (items.id=202, 2026-08-04): the curated Tier 2/Tier 3
-- provider catalog backing TIER3_ACCESS_MODEL.md's selector screen (State
-- 3, decisions.id=681). Per decisions.id=684 (schema required, scoped
-- together with items.id=186's curation policy) and decisions.id=710
-- (items.id=186 scoped -- policy this schema expresses).
--
-- WHY shared.db, NOT integration_keys.db (keys_001.sql): this is
-- non-secret catalog data, identical across every install, not per-user
-- credential data -- shared.db's own header ("not per-user encrypted --
-- must be readable before any user logs in") is the correct fit, unlike
-- integration_keys.db's per-user SQLCipher-encrypted credential rows.
--
-- WHY ADDED HERE, NOT a new shared_003.sql: Jason's direction, 2026-08-04
-- -- consolidated directly into this file rather than as a separate
-- migration, consistent with this file's own established "pre-release,
-- zero shipped users" precedent (see CONSOLIDATION NOTE above) for schema
-- additions that don't need to walk an existing install through an
-- intermediate state.
--
-- Release-bundled per decisions.id=710(b) (no remote/server-fetched list
-- for R1) -- rows are seeded by app releases, not created at runtime by
-- users.
--
-- CARD DISPLAY: loosely based on Active Board's card anatomy
-- (decisions.id=377), NOT a literal reuse. Active Board's shared skeleton
-- (Persona color bar, name, small badge icon top-right, primary CTA
-- bottom-right) carries over in spirit (display_name -> card title,
-- login_required -> badge cue, a CTA to add/select), but decisions.id=377
-- explicitly lists "privacy label text" as something that NEVER appears
-- on an Active Board card surface -- the exact opposite of what this
-- card's own purpose requires (TIER3_ACCESS_MODEL.md's two-box framing,
-- "no login required" / "account required, data retained", is built
-- entirely around surfacing that information). Flagging this explicitly
-- so a future reader doesn't read "loosely based on" as closer than it
-- is: the retention/documentation-gate fields below are meant to render
-- directly on this card, which Active Board's own convention forbids for
-- its own card type.
--
-- COLUMN DESIGN, per decisions.id=710's policy (concrete columns left to
-- Chat-DEV per that decision's own text):
--   tier: 2 or 3 -- INTEGER not TEXT, closed 2-value domain, CHECK-
--     enforced (unlike keys_001.sql's open key_type, whose value set is
--     expected to grow with Phase 2 integrations -- tier here is exactly
--     TIER3_ACCESS_MODEL.md's two lanes, a closed set by the spec itself).
--   mode: 'embedded_web' | 'api' -- CHECK-enforced closed set. Only
--     'embedded_web' is used for R1 (TIER3_ACCESS_MODEL.md's Manual
--     mode); 'api' reserved so a future Managed/API build (items.id=151)
--     extends this table rather than needing a parallel one. Mode-
--     specific fields (e.g. an API base URL/auth shape) are NOT designed
--     here -- out of scope until items.id=151 is actually scheduled.
--   launch_url: the embedded_web pane's target URL. NULL when mode='api'.
--   login_required: INTEGER boolean. Drives the selector's two box labels
--     directly -- kept as its own explicit column rather than inferred
--     from tier, in case a future tier assignment and login status ever
--     diverge.
--   activation_status: 'active' | 'deprecated'. Deliberately NOT a richer
--     state machine (no 'pending'/'suspended' etc.): decisions.id=710(b)
--     confirmed the list is release-bundled, not runtime-activated, so
--     there is no window where a row exists but isn't yet live. Mirrors
--     keys_001.sql's is_active INTEGER simplicity over inventing states
--     the actual lifecycle doesn't need.
--   documentation_gate: TEXT, JSON, default '{}'. Space for decisions.id=
--     710(a)'s documentation/transparency gate criteria (ToS/retention
--     policy citation, disclosed jurisdiction, note on any known
--     contradictory third-party reporting) -- also the source for this
--     card's retention-posture display fields (see CARD DISPLAY above).
--     Freeform JSON mirrors extra_metadata's escape-hatch convention
--     (keys_001.sql, personas.extra_metadata) rather than dedicated
--     columns per field: this is qualitative, descriptive content, not
--     values something would query/filter on.
--   last_reviewed_at: TEXT (ISO8601), nullable. Records WHEN a provider
--     was last reviewed per decisions.id=710's schema implication --
--     does NOT itself drive any scheduling logic (no cron/cadence
--     mechanism here, matching 710(c)'s explicit rejection of
--     fixed-schedule re-review -- the trigger is a real signal, not a
--     timer reading this column).
--   review_trigger_note: TEXT, nullable. Freeform note on what triggered
--     the most recent review (e.g. "Groq ToS vs. third-party retention
--     discrepancy, 2026-07-29" per decisions.id=710(a)'s own cautionary
--     precedent) -- audit-trail content, plain TEXT not JSON since it's a
--     single freeform note rather than structured data.
--
-- Seed data: NOT included here. Populating real, verified rows for
-- Duck.ai/Brave Leo/Claude/ChatGPT/Gemini against decisions.id=710(a)'s
-- documentation-gate criteria is research/content work (decisions.id=684's
-- own "real research work, not app-install configuration" framing),
-- explicitly out of scope for this schema addition -- flagged in handoff.
CREATE TABLE IF NOT EXISTS tier3_providers (
    id                      TEXT PRIMARY KEY,
    display_name            TEXT NOT NULL,
    tier                    INTEGER NOT NULL CHECK (tier IN (2, 3)),
    mode                    TEXT NOT NULL DEFAULT 'embedded_web'
                                CHECK (mode IN ('embedded_web', 'api')),
    launch_url              TEXT,
    login_required          INTEGER NOT NULL CHECK (login_required IN (0, 1)),
    activation_status       TEXT NOT NULL DEFAULT 'active'
                                CHECK (activation_status IN ('active', 'deprecated')),
    documentation_gate      TEXT NOT NULL DEFAULT '{}',
    last_reviewed_at        TEXT,
    review_trigger_note     TEXT,
    created_at              TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tier3_providers_selector
    ON tier3_providers (activation_status, tier, login_required);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (1, datetime('now'),
    'shared.db schema (consolidated 2026-07-24, items.id=169; auth foundation consolidated 2026-08-01, items.id=205; tier3_providers added 2026-08-04, items.id=202): personas, users, user_salts, user_capabilities, artifact_versions, topic_index, asset_index, focus_settings, tier3_providers + dev seeds');
