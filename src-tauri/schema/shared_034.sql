-- shared_034.sql
--
-- items.id=630 (decisions.id=893, AUTH_MULTIUSER_ARCHITECTURE.md Section 4.0):
-- state for data-key (DEK) rotation, which every password change, restore
-- from the recovery phrase and new recovery phrase action performs.
--
-- user_key_rotations: one row per rotation. At most one rotation per user is
-- active (not committed and not rolled back), enforced by the partial unique
-- index below. While a rotation is active, user_key_wrappers.pending_rotation
-- (shared_033.sql) holds its rotation_id.
--   state: the Architecture 8.6 vocabulary. pending, in_progress, verifying,
--     committed, rolled_back, failed. verifying means the new wraps are live
--     (promotion step 1 ran) and the staging below is kept until the read-back
--     unlock succeeds. failed keeps the row active so it can be resumed.
--   direction: forward, or rollback while an automatic rollback runs.
--   old_dek_under_new_hex: the OLD data key encrypted under the NEW data key
--     (ChaCha20-Poly1305, authenticated data binds user, epochs and rotation).
--     Whoever can reach the new key (new password or recovery phrase, through
--     the staged wraps) can finish or undo databases already moved.
--   staged_*: the values promotion step 1 copies into the live wrapper, the
--     user_salts row and user_sharing_keys. Computed up front so promotion is
--     a pure copy inside one transaction.
-- All secret columns are set to NULL when the rotation finishes or rolls back.
--
-- user_key_rotation_items: one row per database file or blob the rotation
-- re-keys, with its own state. The row is a hint only, the file itself is the
-- truth on resume (a crash can fall between the re-key and the row update).
-- rel_path is relative to users/ then the user id.
--
-- user_sharing_keys gains the key epoch its public key belongs to, so a sender
-- or the group re-wrap (items.id=659) can tell which epoch a key is from.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals or comments -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS user_key_rotations (
    rotation_id                   TEXT PRIMARY KEY,
    user_id                       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind                          TEXT NOT NULL
                                      CHECK (kind IN ('password_change', 'restore', 'new_phrase')),
    direction                     TEXT NOT NULL DEFAULT 'forward'
                                      CHECK (direction IN ('forward', 'rollback')),
    state                         TEXT NOT NULL DEFAULT 'pending'
                                      CHECK (state IN ('pending', 'in_progress', 'verifying',
                                                       'committed', 'rolled_back', 'failed')),
    from_epoch                    INTEGER NOT NULL CHECK (from_epoch >= 1),
    to_epoch                      INTEGER NOT NULL CHECK (to_epoch > from_epoch),
    started_at                    TEXT NOT NULL,
    finished_at                   TEXT,
    last_error                    TEXT,
    old_dek_under_new_hex         TEXT,
    staged_salt_hex               TEXT,
    staged_kdf_memory_kib         INTEGER,
    staged_kdf_iterations         INTEGER,
    staged_kdf_parallelism        INTEGER,
    staged_format_version         INTEGER,
    staged_password_wrap_hex      TEXT,
    staged_recovery_public_key_hex TEXT,
    staged_recovery_seal_hex      TEXT,
    staged_sharing_public_key_hex TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_user_key_rotations_one_active
    ON user_key_rotations (user_id)
    WHERE state NOT IN ('committed', 'rolled_back');

CREATE TABLE IF NOT EXISTS user_key_rotation_items (
    rotation_id   TEXT NOT NULL REFERENCES user_key_rotations(rotation_id) ON DELETE CASCADE,
    rel_path      TEXT NOT NULL,
    kind          TEXT NOT NULL CHECK (kind IN ('db', 'blob')),
    state         TEXT NOT NULL DEFAULT 'pending'
                      CHECK (state IN ('pending', 'in_progress', 'verifying',
                                       'committed', 'rolled_back', 'failed')),
    bytes         INTEGER NOT NULL DEFAULT 0,
    error         TEXT,
    PRIMARY KEY (rotation_id, rel_path)
);

ALTER TABLE user_sharing_keys ADD COLUMN key_epoch INTEGER NOT NULL DEFAULT 1;
ALTER TABLE user_sharing_keys ADD COLUMN rotated_at TEXT;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (34, datetime('now'),
    'items.id=630: user_key_rotations, user_key_rotation_items and user_sharing_keys.key_epoch -- state for data-key rotation (decisions.id=893)');
