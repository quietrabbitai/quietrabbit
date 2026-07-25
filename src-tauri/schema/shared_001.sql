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
CREATE TABLE IF NOT EXISTS users (
    id                          TEXT PRIMARY KEY,
    display_name                TEXT NOT NULL UNIQUE,
    role                        TEXT NOT NULL DEFAULT 'builder'
                                    CHECK (role IN ('consumer', 'builder', 'admin')),
    is_primary                  INTEGER NOT NULL DEFAULT 0,
    auth_enabled                INTEGER NOT NULL DEFAULT 0,
    password_hash               TEXT,
    tier2_provider_preference   TEXT
                                    CHECK (tier2_provider_preference IS NULL
                                        OR tier2_provider_preference IN ('mistral', 'groq')),
    created_at                  TEXT NOT NULL,
    extra_metadata              TEXT NOT NULL DEFAULT '{}'
);

-- Enforce only one primary user per instance
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_single_primary
    ON users (is_primary) WHERE is_primary = 1;

-- User salts — includes KDF metadata for future algorithm upgrades
CREATE TABLE IF NOT EXISTS user_salts (
    user_id         TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    salt_hex        TEXT NOT NULL,
    kdf_algorithm   TEXT NOT NULL DEFAULT 'pbkdf2_sha256',
    kdf_iterations  INTEGER NOT NULL DEFAULT 600000,
    created_at      TEXT NOT NULL
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

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (1, datetime('now'),
    'shared.db schema (consolidated 2026-07-24, items.id=169): personas, users, artifact_versions, topic_index, asset_index, focus_settings + dev seeds');
