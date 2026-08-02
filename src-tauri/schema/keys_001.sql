-- persistence/schema/keys_001.sql
-- Per-user integration keys database schema: integration_keys.db
-- Encrypted with SQLCipher using user master key.
-- Path: /users/{user_id}/integration_keys.db
-- Migration version: 1
--
-- EDITED DIRECTLY, NOT VERSIONED (items.id=205, 2026-08-01, Jason's
-- direction): this file originally stored credentials with an additional
-- field-level encryption layer (encrypted_key BLOB + iv_hex TEXT) on top
-- of SQLCipher. That layer is removed here, and persona_id/auth_type/
-- expires_at (Architecture/AUTH_MULTIUSER_ARCHITECTURE.md Section 8.2) are
-- added, by editing this file directly rather than adding a keys_002.sql
-- rebuild migration on top of it. QR has no shipped users and no on-disk
-- data in the old format (decisions.id=675) -- shared_001.sql's own
-- CONSOLIDATION NOTE (items.id=169, 2026-07-24) already established this
-- exact precedent for pre-release schema files: "Pre-release, zero shipped
-- users -- consolidated directly to final naming, skipping the
-- intermediate relay." A versioned rebuild migration is the correct tool
-- for changing the shape of a table an existing install has real rows in;
-- this file has never been applied by any real install (verified this
-- session: zero Rust code anywhere reads/writes encrypted_key, iv_hex,
-- persona_id, auth_type, or expires_at), so there is no intermediate state
-- for a migration to walk through -- only a final shape to describe.
--
-- FIELD-LEVEL ENCRYPTION REMOVED -- SECTION 8.1 CALL (items.id=205 scoping,
-- 2026-08-01, Chat-DEV): Architecture Section 8.1 resolves the
-- decisions.id=65 vs. 432 table-SHAPE conflict only (per-user vs.
-- shared.db) -- it does not address encrypted_key/iv_hex's field-level
-- layer directly. That layer's original rationale (decisions.id=65: fresh
-- IV per write "prevents correlation attacks") does not add real defense
-- once DB-level SQLCipher is the reconfirmed base (Section 4.1/4.2): every
-- byte of this file, including this column, is already ciphertext without
-- the master key -- an attacker who can read a decrypted row already has
-- the master key and therefore the plaintext credential directly. No
-- second key for a field-level layer is specified anywhere in the
-- architecture document. Verified this session: zero Rust code anywhere
-- implements encrypted_key/iv_hex, and personal_store.rs's own header
-- describes the identical deferred pattern as "Layer 8" -- Section 13 of
-- the architecture document retires "Layer 8" as a phase label entirely.
-- Dropped as redundant, consistent with the rest of the codebase, not just
-- this table.
--
-- credential TEXT NOT NULL replaces encrypted_key/iv_hex -- plain storage,
-- relies solely on SQLCipher's file-level encryption of integration_keys.db.
--
-- persona_id / auth_type / expires_at (Section 8.2): persona_id nullable
-- (NULL = user-global key). auth_type constrained via CHECK to the three
-- values Section 8.2 gives explicitly ('api_key' | 'oauth_token' |
-- 'manual_copy') -- a closed, specified set, unlike key_type below, so a
-- CHECK catches a typo'd value at write time rather than silently
-- accepting it. expires_at nullable -- OAuth tokens expire, static keys do
-- not.
--
-- UNIQUE (provider, key_type, integration_id, persona_id) -- INCLUDES
-- persona_id, Jason's explicit direction (2026-08-01): a user may hold a
-- personal-use key and a separate Work-persona key for the same provider
-- simultaneously (e.g. two distinct Gemini keys, one global/personal, one
-- persona-scoped) -- these are legitimately different credentials, not a
-- duplicate, so persona_id must be part of the uniqueness scope. A global
-- key (persona_id IS NULL) and a persona key coexisting is the exact
-- scenario this constraint exists to allow, not a state to prevent -- no
-- "exactly one row wins" resolution logic is implied for this table (unlike
-- shared_003.sql's user_capabilities), so no partial-index NULL-handling
-- split is needed here.
--
-- key_type: open TEXT — no CHECK constraint.
--   Current values: 'tier2', 'tier3'
--   Phase 2 additions: 'integration' (Notion, Calendar, GitHub, etc.)
--   Application layer validates key_type values.
-- integration_id: distinguishes multiple integrations from same provider
--   (e.g., Google Drive + Gmail = same provider, different integration_id).

CREATE TABLE IF NOT EXISTS schema_version (
    version         INTEGER PRIMARY KEY,
    applied_at      TEXT NOT NULL,
    description     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS migration_lock (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    locked_at   TEXT,
    locked_by   TEXT
);
INSERT OR IGNORE INTO migration_lock (id) VALUES (1);

CREATE TABLE IF NOT EXISTS integration_keys (
    id                  TEXT PRIMARY KEY,
    provider            TEXT NOT NULL,
    key_type            TEXT NOT NULL,
    integration_id      TEXT NOT NULL DEFAULT '_default',
    credential_label    TEXT NOT NULL,
    credential          TEXT NOT NULL,
    persona_id          TEXT REFERENCES personas(id) ON DELETE CASCADE,
    auth_type           TEXT
                            CHECK (auth_type IS NULL
                                   OR auth_type IN ('api_key', 'oauth_token', 'manual_copy')),
    expires_at          TEXT,
    is_active           INTEGER NOT NULL DEFAULT 1,
    created_at          TEXT NOT NULL,
    last_verified_at    TEXT,
    extra_metadata      TEXT NOT NULL DEFAULT '{}',
    UNIQUE (provider, key_type, integration_id, persona_id)
);

CREATE INDEX IF NOT EXISTS idx_integration_keys_lookup
    ON integration_keys (provider, key_type, is_active, last_verified_at);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (1, datetime('now'), 'integration_keys.db schema (items.id=205, 2026-08-01: credential/persona_id/auth_type/expires_at, field-level encryption removed per Section 8.1)');
