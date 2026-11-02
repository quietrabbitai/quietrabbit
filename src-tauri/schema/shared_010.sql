-- shared_010.sql
--
-- items.id=304: VIEW-ONLY cross-account persona sharing (decisions.id=723).
-- Split from items.id=300's resolved design. Reuses items.id=303's
-- pending_persona_shares grant/envelope table and persona_share_sync_settings
-- push machinery as-is (see persona_view_sync::engine's own module header)
-- rather than forking a parallel grant table -- the send/accept envelope
-- shape (recipient addressing, X25519 encryption, pending/accepted lifecycle,
-- discoverable in shared.db before login) is identical between SYNCED and
-- VIEW-ONLY. Only accept-time behavior differs (materialize a Persona vs.
-- populate a read-only cache), which lives in Rust, not schema.
--
-- share_type: distinguishes the two grant types on an already-shipped table.
-- NOT NULL DEFAULT 'synced', no CHECK constraint -- SQLite's ALTER TABLE ADD
-- COLUMN cannot cleanly add a CHECK against an existing table (would require
-- a full table rebuild); shared_008.sql already established the precedent of
-- validating an added column's value set in Rust instead (this one via a new
-- ShareType::parse, mirroring persona_sync::settings_store::SyncRole::parse).
-- DEFAULT is never actually relied upon -- pre-release, zero shipped rows,
-- every future send_persona_share call supplies a real value explicitly.
--
-- revoked_at: the owner's own record that they revoked a VIEW-ONLY share
-- (decisions.id=723's tombstone design). Nullable, no CHECK needed -- avoids
-- touching pending_persona_shares.status's existing CHECK ('pending' /
-- 'accepted' / 'declined'), which SQLite also cannot alter without a table
-- rebuild. A revoked share stays status='accepted' (it did happen) with
-- revoked_at set; persona_view_sync::engine's push path checks this column to
-- decide whether to emit a Content or Revoked payload. Meaningless for
-- share_type='synced' -- SYNCED revocation is a separate, deliberately
-- unresolved concern (decisions.id=617's asymmetric framing, items.id=299
-- point 4), not this column.
--
-- persona_view_share_settings: the VIEW-ONLY recipient's own folder-sync
-- bookkeeping, keyed by (recipient_user_id, share_id) because a VIEW-ONLY
-- recipient has no persona_id for the share. items.id=661 (decisions.id=862)
-- moved this table out of the plaintext shared.db into the recipient's
-- encrypted integration_keys.db (keys_006.sql): the folder path is a
-- user-chosen location and last_error can embed one. This file is edited in
-- place, which decisions.id=857 allows until first ship.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE pending_persona_shares ADD COLUMN share_type TEXT NOT NULL DEFAULT 'synced';
ALTER TABLE pending_persona_shares ADD COLUMN revoked_at TEXT;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (10, datetime('now'),
    'items.id=304: pending_persona_shares.share_type/.revoked_at -- VIEW-ONLY persona-sharing grant type (persona_view_share_settings since moved to the encrypted integration_keys.db by items.id=661)');
