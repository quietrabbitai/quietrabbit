-- shared_004.sql
--
-- items.id=289: account-creation asymmetric keypair (decisions.id=677,
-- 2026-07-29) -- approved but never built until now (confirmed this session:
-- zero keypair/public_key references anywhere under src-tauri/src/auth/, no
-- reserved column in shared_001.sql). Foundation for both SYNCED
-- persona-sharing (decisions.id=617) and group.db invitations
-- (items.id=189/283/284, pending_group_invitations.encrypted_group_key in
-- shared_003.sql already assumes this mechanism exists).
--
-- WHY A NEW FILE, NOT edited into shared_001.sql: same reasoning
-- shared_002.sql/shared_003.sql already give -- a genuinely separate,
-- independently-shipped feature landing well after the original
-- consolidation, continuing the existing version sequence.
--
-- WHY A NEW TABLE, NOT a column on users: modeled directly on the
-- users/user_salts split (shared_001.sql) -- keeps `users` itself lean and
-- groups an account's cryptographic material together, same rationale that
-- split already established.
--
-- WHY shared.db: the public key is explicitly not secret (decisions.id=677:
-- "fine to be readable instance-wide") -- same fit as the rest of shared.db's
-- unencrypted, instance-wide, readable-before-login data.
--
-- public_key_hex: the X25519 public key, hex-encoded TEXT -- matches this
-- schema family's existing convention (user_salts.salt_hex,
-- pending_group_invitations.encrypted_group_key) of storing encoded
-- key/ciphertext material as TEXT rather than BLOB. NOT secret -- no
-- encryption, no access restriction beyond what shared.db already has.
--
-- The private key is deliberately NOT stored anywhere, encrypted or
-- otherwise (see src-tauri/src/auth/sharing_keypair.rs's own module header):
-- it's derived from the account's master key via HKDF-SHA256 on demand each
-- login, the same way the master key itself is re-derived from the password
-- each login rather than persisted. A stored encrypted copy would need a
-- second key to encrypt it with -- which would just be the master key again,
-- adding a ciphertext with no real defense-in-depth value.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS user_sharing_keys (
    user_id             TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    public_key_hex      TEXT NOT NULL,
    created_at          TEXT NOT NULL
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (4, datetime('now'),
    'items.id=289: user_sharing_keys -- shared.db public-key storage for the account-creation X25519 keypair (decisions.id=677)');
