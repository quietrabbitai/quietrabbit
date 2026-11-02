-- persistence/schema/keys_006.sql
-- Per-user integration keys database schema: integration_keys.db
-- Migration version: 6
--
-- items.id=661 (decisions.id=862): sensitive metadata that lived in the
-- plaintext shared.db moves into this per-user encrypted file. shared.db is
-- opened with no key so the login screen can read it, which means anything in
-- it is readable from disk without the password. These tables hold the
-- user-authored or revealing part of what shared.db used to hold:
--
--   persona_registry          Persona display name, type and colour
--                             (shared.db personas.display_name, persona_type
--                             and extra_metadata.color). shared.db keeps only
--                             personas(id, created_at) as the FK anchor.
--   user_provider_preference  which providers a user uses per Focus, Persona
--                             and account, with the chosen plan and model
--                             (shared_013.sql, shared_026.sql).
--   group_sync_settings       folder path and last outcome per (persona,
--                             group) (shared_005.sql).
--   persona_share_sync_settings
--                             folder path and sync bookkeeping per (persona,
--                             share) (shared_009.sql).
--   persona_view_share_settings
--                             folder path per recipient view share
--                             (shared_010.sql).
--
-- A new numbered migration, not an edit of an older keys file, for the same
-- reason as keys_003.sql: open_integration_keys_db always runs the migrator,
-- so existing databases pick this file up. The shared.db side of the move is
-- done by editing the shared baselines in place (decisions.id=857), which
-- resets existing dev databases once.
--
-- The column lists are copied from the shared.db tables so the stores keep
-- their SQL. Two differences: the foreign keys to users, personas and
-- providers are gone (those rows live in shared.db, a separate SQLite file;
-- all ids here are plain TEXT, validated by the application layer, as in
-- keys_003.sql), and user_provider_preference gains plan_key and model_id as
-- ordinary columns instead of a later ALTER. user_id stays in
-- user_provider_preference and recipient_user_id in
-- persona_view_share_settings: they are redundant in a per-user file, but
-- keeping them leaves the stores' SQL, unique indexes and the specta types
-- unchanged.
--
-- A last_error value is engine or I/O error text and can embed a path, which
-- is why the settings tables carry it here with the path rather than leaving
-- it behind in shared.db.
--
-- No persona_registry CHECK on persona_type: the application layer limits it
-- to standard or protected, and fixtures use other values, exactly as the
-- shared.db column did.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS persona_registry (
    persona_id      TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    persona_type    TEXT NOT NULL,
    color           TEXT,
    created_at      TEXT NOT NULL
);

-- user_provider_preference design notes, carried over from the comment that
-- sat above this table in shared_013.sql (items.id=427/428, Part 2b):
--   SCOPE: User x Persona x Focus. The account-wide row (persona_id and
--   focus_id both NULL) is the comprehensive record. A Persona or Focus row is
--   a stub that exists only when that scope needs to diverge from what it
--   would inherit.
--   NEW-USER DEFAULT: the absence of any row at any scope means a system-wide
--   hardcoded default applies, never inheritance from another user's rows.
--   UNIQUENESS: a composite primary key cannot stop two account-wide rows for
--   the same (user_id, provider_id) because SQLite treats NULLs as distinct,
--   so each scope level has a partial unique index below.
--   subscription_status: paid or free tier per provider. Nullable, nothing
--   branches on it yet.
--   last_verified_at mirrors get_capability_profile's live-check-never-cached
--   convention. It is not a trusted cache.
CREATE TABLE IF NOT EXISTS user_provider_preference (
    id                    TEXT PRIMARY KEY,
    user_id               TEXT NOT NULL,
    persona_id            TEXT,
    focus_id              TEXT,
    provider_id           TEXT NOT NULL,
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
    created_at            TEXT NOT NULL,
    plan_key              TEXT,
    model_id              TEXT
);

-- Account-wide row uniqueness (persona_id and focus_id both NULL).
CREATE UNIQUE INDEX IF NOT EXISTS idx_user_provider_pref_account
    ON user_provider_preference (user_id, provider_id)
    WHERE persona_id IS NULL AND focus_id IS NULL;

-- Persona-wide row uniqueness (persona_id set, focus_id NULL).
CREATE UNIQUE INDEX IF NOT EXISTS idx_user_provider_pref_persona
    ON user_provider_preference (user_id, persona_id, provider_id)
    WHERE persona_id IS NOT NULL AND focus_id IS NULL;

-- Focus-specific row uniqueness (focus_id set already pins a single Persona).
CREATE UNIQUE INDEX IF NOT EXISTS idx_user_provider_pref_focus
    ON user_provider_preference (user_id, focus_id, provider_id)
    WHERE focus_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_user_provider_pref_lookup
    ON user_provider_preference (user_id, provider_id);

CREATE TABLE IF NOT EXISTS group_sync_settings (
    persona_id      TEXT NOT NULL,
    group_id        TEXT NOT NULL,
    folder_path     TEXT NOT NULL,
    last_synced_at  TEXT,
    last_error      TEXT,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (persona_id, group_id)
);

CREATE TABLE IF NOT EXISTS persona_share_sync_settings (
    persona_id          TEXT NOT NULL,
    share_id            TEXT NOT NULL,
    role                TEXT NOT NULL CHECK (role IN ('owner', 'recipient')),
    folder_path         TEXT NOT NULL,
    last_synced_at      TEXT,
    last_pushed_at      TEXT,
    last_content_hash   TEXT,
    last_error          TEXT,
    updated_at          TEXT NOT NULL,
    PRIMARY KEY (persona_id, share_id)
);

CREATE TABLE IF NOT EXISTS persona_view_share_settings (
    recipient_user_id   TEXT NOT NULL,
    share_id            TEXT NOT NULL,
    folder_path         TEXT NOT NULL,
    last_error          TEXT,
    updated_at          TEXT NOT NULL,
    PRIMARY KEY (recipient_user_id, share_id)
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (6, datetime('now'), 'items.id=661 (decisions.id=862): persona_registry, user_provider_preference, group_sync_settings, persona_share_sync_settings and persona_view_share_settings -- sensitive metadata moved out of plaintext shared.db into the per-user encrypted file');
