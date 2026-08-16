-- shared_003.sql
--
-- items.id=283 (266a): pending-invitation-envelope table for group.db
-- sharing (Working/GROUP_DB_DESIGN_20260802.md Section 2.1). items.id=189's
-- 2026-08-16 resolution: group invitations need Architecture Section 4.3's
-- asymmetric keypair mechanism (already built for decisions.id=617 SYNCED
-- persona sharing) -- VIEW-ONLY sharing does not, it's a structurally
-- different problem. Transport format resolved the same day: a new table
-- here in shared.db, keyed by recipient, holding pending encrypted group-key
-- envelopes -- not the group-sync folder (Section 2.4), since this is
-- account-to-account key exchange, not group content.
--
-- WHY shared.db, NOT group.db: the recipient does not have the group's key
-- yet -- that is precisely what this table exists to deliver -- so it
-- cannot live inside an encrypted-under-that-key group.db. shared.db is
-- unencrypted and instance-wide, readable before any user logs in (see
-- shared_001.sql's own header), the correct fit for a table the recipient's
-- own accept flow must be able to read before their group key exists.
--
-- WHY A NEW FILE, NOT edited into shared_001.sql/shared_002.sql: this is a
-- genuinely separate, independently-shipped feature landing well after
-- those two -- same reasoning shared_002.sql itself gives for not folding
-- into shared_001.sql's consolidation. Continues the existing
-- shared_001/shared_002 version sequence; neither existing file is touched.
--
-- SCOPE: schema only. items.id=284 owns the actual encrypt/send/accept
-- logic that reads and writes this table -- nothing in this migration
-- populates or consumes it.
--
-- recipient_persona_id: references personas(id) -- always a LOCAL Persona,
-- since an invitation is addressed to a Persona on *this* install (the
-- recipient's own accept flow runs here). ON DELETE CASCADE matches
-- focus_settings_friction_decisions' and other persona-scoped tables'
-- existing convention.
--
-- group_id: deliberately no FK -- group.db is a separate encrypted file,
-- never tracked as a row in shared.db, so there is nothing local to
-- reference. Opaque identifier, trusted only because it arrived inside a
-- real invitation envelope.
--
-- group_display_name: so the recipient's accept-flow UI can identify what
-- they're being invited to before they've decrypted anything -- the
-- envelope itself (encrypted_group_key) reveals nothing until accepted.
--
-- encrypted_group_key: the ciphertext -- the group's symmetric key,
-- encrypted specifically to this recipient Persona's public key
-- (Architecture/AUTH_MULTIUSER_ARCHITECTURE.md Section 4.3's asymmetric
-- keypair mechanism). TEXT, matching this schema family's existing
-- convention of storing encoded ciphertext/blob material as TEXT rather
-- than BLOB (see integration_keys.db's own credential column).
--
-- sender_label: opaque, freeform identifier for who sent the invitation.
-- Deliberately NOT a FK to users/personas -- per design doc Section 2.1,
-- the sender may be a different QR install/instance entirely (household
-- and business-team members each run their own install), so there is no
-- local row to reference in the general case.
--
-- status / responded_at: exist so items.id=284's accept flow has somewhere
-- to record the outcome -- no accept/decline logic is implemented by this
-- migration, just the columns. 'accepted'/'declined' only (no 'expired'):
-- no automatic-expiry mechanism is designed anywhere in the source design
-- doc, matching this project's general preference (see items.id=210's
-- checkout-lock resolution, same day) for not inventing timeout machinery
-- nothing has asked for yet.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS pending_group_invitations (
    id                      TEXT PRIMARY KEY,
    recipient_persona_id    TEXT NOT NULL REFERENCES personas(id) ON DELETE CASCADE,
    group_id                TEXT NOT NULL,
    group_display_name      TEXT NOT NULL,
    encrypted_group_key     TEXT NOT NULL,
    sender_label            TEXT NOT NULL,
    status                  TEXT NOT NULL DEFAULT 'pending'
                                CHECK (status IN ('pending', 'accepted', 'declined')),
    created_at              TEXT NOT NULL,
    responded_at            TEXT,
    extra_metadata          TEXT NOT NULL DEFAULT '{}'
                                CHECK (json_valid(extra_metadata))
);

CREATE INDEX IF NOT EXISTS idx_pending_group_invitations_recipient
    ON pending_group_invitations (recipient_persona_id, status);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (3, datetime('now'),
    'items.id=283 (266a): pending_group_invitations -- shared.db envelope table for group-key invitation transport (items.id=189)');
