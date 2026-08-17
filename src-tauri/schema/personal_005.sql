-- personal_005.sql
--
-- items.id=290 (group.db 266g): durable group-key storage. GroupKeyRegistry
-- (items.id=283, auth/registry.rs) is deliberately volatile Tauri-managed
-- state -- on app restart or logout, every accepted group key it held is
-- gone, with no way to recover short of being re-invited. decisions.id=718
-- resolves this: group_keys is written alongside every existing
-- GroupKeyRegistry::replace call (auth::group_invitations::accept_invitation,
-- auth::group_membership::apply_pending_rotations), read back at login to
-- rehydrate the registry (commands::auth::finish_login), and cleaned up on
-- the one real eviction call site (auth::group_membership::remove_member's
-- own-departure path).
--
-- NO NEW CRYPTO PRIMITIVE: personal.db is already opened under the
-- account's resident master key (KeyRegistry, same key_hex used for
-- outputs.db/integration_keys.db) -- there is no per-field double-encryption
-- anywhere in this codebase. Storing the group key as plaintext-within-the-
-- encrypted-file gets it identical protection to every other personal.db
-- table, for free. Considered and rejected: a new symmetric AEAD wrap/unwrap
-- primitive under a master-key-derived wrapping key -- solves nothing
-- personal.db's existing SQLCipher-at-rest model doesn't already solve.
--
-- NO group_id -> group_key MAPPING TABLE ELSEWHERE, and no persona_id
-- COLUMN HERE: personal.db is already one file per (user_id, persona_id)
-- pair -- path is /users/{user_id}/personas/{persona_id}/personal.db. Every
-- existing table in this file (entities, entity_facts, voice_profiles,
-- document_forks, ...) omits persona_id for the same reason
-- (personal_001.sql's own header: "user_id and persona_id are encoded in
-- the file path"). group_keys follows that same convention.
--
-- group_id is the PRIMARY KEY, not a surrogate id column: one resident key
-- per group per persona is the entire invariant this table exists to hold
-- (matches GroupKeyRegistry's own (persona_id, group_id) keying, with
-- persona_id already implicit in the file). Both write points
-- (group_key_store::save_group_key) upsert on this key -- a persona
-- accepting an invitation and later applying a rotation for the same group
-- both legitimately retarget the same row, not append a new one.
--
-- group_key_hex, not a BLOB: matches every other hex-encoded key value in
-- this codebase (auth::registry::key_hex's own convention) -- callers
-- already have the group key in this form (either freshly decrypted in
-- accept_invitation/apply_pending_rotations, or read back here to feed
-- straight into GroupKeyRegistry::replace via the same hex round-trip).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS group_keys (
    group_id        TEXT NOT NULL PRIMARY KEY,
    group_key_hex   TEXT NOT NULL,
    created_at      TEXT NOT NULL
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (5, datetime('now'),
    'items.id=290: group_keys -- durable storage for GroupKeyRegistry entries (decisions.id=718)');
