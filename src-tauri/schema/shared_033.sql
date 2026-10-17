-- shared_033.sql
--
-- items.id=629 (decisions.id=893, AUTH_MULTIUSER_ARCHITECTURE.md Section 4.0):
-- the key wrapper. Per account, beside user_salts, so the whole-directory
-- backup rule of Section 11 already covers it. The wrapper is what lets the
-- password or the recovery phrase recover the random data key (DEK) the
-- databases are encrypted under. Nothing here opens any data without the
-- password or the 24-word phrase.
--
-- password_wrap_hex: the DEK encrypted under the Argon2id password key with
-- ChaCha20-Poly1305, authenticated data = user id, key epoch and format
-- version (byte layout in src-tauri/src/auth/data_key.rs). Its tag is the
-- password check.
-- recovery_public_key_hex: X25519 public key derived from the phrase's
-- entropy. Public, the matching private key and the entropy are never stored.
-- recovery_seal_hex: the DEK sealed to that public key.
-- key_epoch: counter that increases at every rotation. Every store open
-- compares the epoch its session holds against this column.
-- format_version: wrapper format version, bound into the wrap's authenticated
-- data.
-- pending_rotation: marker for the rotation state machine (items.id=630).
-- Always NULL until that item lands.
--
-- Pre-existing dev accounts have no row here and cannot be unlocked
-- (one-time dev data reset, no shipped users).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals or comments -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS user_key_wrappers (
    user_id                  TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    key_epoch                INTEGER NOT NULL DEFAULT 1 CHECK (key_epoch >= 1),
    format_version           INTEGER NOT NULL,
    password_wrap_hex        TEXT NOT NULL,
    recovery_public_key_hex  TEXT NOT NULL,
    recovery_seal_hex        TEXT NOT NULL,
    pending_rotation         TEXT,
    created_at               TEXT NOT NULL,
    updated_at               TEXT NOT NULL
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (33, datetime('now'),
    'items.id=629: user_key_wrappers -- password wrap, recovery public key, recovery seal and key epoch for the data key (decisions.id=893)');
