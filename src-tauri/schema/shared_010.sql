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
-- bookkeeping. NOT a reuse of persona_share_sync_settings (shared_009.sql):
-- that table's PK is (persona_id, share_id), and a VIEW-ONLY recipient has no
-- persona_id at all for this share -- decisions.id=723's whole point is that
-- no Persona is ever created. The VIEW-ONLY *owner* side is structurally
-- identical to SYNCED's owner side (a real persona_id that pushes), so owner
-- rows reuse persona_share_sync_settings (role='owner') unchanged; only the
-- recipient side needed a new shape, keyed by (recipient_user_id, share_id)
-- instead.
--
-- Deliberately NO last_synced_at column here (contrast with
-- persona_share_sync_settings' recipient rows) -- unlike SYNCED, VIEW-ONLY's
-- content lives in exactly one place, the encrypted view_cache.db this same
-- pull already has to open every sweep to check view_cache_meta.status
-- (terminal 'ended' short-circuit, decisions.id=723). Tracking last_synced_at
-- twice, once here and once in view_cache_meta, would just be two copies of
-- the same value with a chance to drift; view_cache_meta.last_synced_at
-- (view_cache_001.sql) is the single source of truth for "is this update
-- newer than the last one applied". This table stays purely install-local
-- mechanics: where to look, and whether the last attempt errored.
--
-- Deliberately NO FK on recipient_user_id/share_id -- matching
-- persona_share_sync_settings' (shared_009.sql) own precedent of plain TEXT
-- id columns, no REFERENCES. This is settings bookkeeping keyed by id, not a
-- relationship shared.db itself needs to enforce.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE pending_persona_shares ADD COLUMN share_type TEXT NOT NULL DEFAULT 'synced';
ALTER TABLE pending_persona_shares ADD COLUMN revoked_at TEXT;

CREATE TABLE IF NOT EXISTS persona_view_share_settings (
    recipient_user_id   TEXT NOT NULL,
    share_id             TEXT NOT NULL,
    folder_path          TEXT NOT NULL,
    last_error           TEXT,
    updated_at           TEXT NOT NULL,
    PRIMARY KEY (recipient_user_id, share_id)
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (10, datetime('now'),
    'items.id=304: pending_persona_shares.share_type/.revoked_at + persona_view_share_settings -- VIEW-ONLY persona-sharing grant type and recipient-side sync bookkeeping');
