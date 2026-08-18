-- shared_007.sql
--
-- items.id=299 (narrowed, 2026-08-17 Chat-PM/Jason): SYNCED persona-sharing
-- grant-flow envelope table. items.id=299 originally scoped five pieces
-- (grant flow, recipient-side acceptance/materialization, ongoing sync
-- wiring, share-stopping, UI); this session's own investigation found that
-- materialization (what "Persona content" physically means, and how it
-- gets decrypted-under-one-master-key and re-encrypted-under-another) and
-- ongoing sync transport are genuinely undesigned -- not just
-- undocumented. Those were split into a dedicated design-pass item
-- (items.id=301, mirroring items.id=210 -> 266 -> 283-292), and this
-- migration covers ONLY the grant-flow half, which items.id=189
-- (2026-08-16) already fully resolved at the transport-mechanism level.
--
-- NOT a reuse of pending_group_invitations (shared_003.sql) -- same
-- reasoning shared_006.sql already gives for creating a second parallel
-- table for key rotation rather than widening an already-shipped table's
-- CHECK constraint for an unrelated lifecycle: this table's status values,
-- addressing shape, and payload shape all differ from the group-invitation
-- case (see per-column notes below).
--
-- WHY shared.db: same reasoning shared_003.sql/shared_004.sql already give
-- -- the recipient must be able to see (though not yet decrypt) a pending
-- share before logging in, and shared.db is the unencrypted, instance-wide,
-- readable-before-login store.
--
-- recipient_user_id, NOT recipient_persona_id (contrast with
-- pending_group_invitations.recipient_persona_id): a SYNCED share's
-- recipient Persona does not exist yet -- decisions.id=617 gives the
-- recipient "their own individually-owned, independently-run instance",
-- materialized only at accept-time (items.id=301's scope, not this
-- migration's). The share is addressed to an account, not a Persona.
--
-- source_persona_id IS a real FK to personas(id), unlike
-- pending_group_invitations.group_id (deliberately no FK there, since a
-- group.db is never tracked as a shared.db row). decisions.id=617 is a
-- household-sharing decision, and shared_003.sql's own header already
-- establishes that this codebase's cross-account sharing operates within
-- one shared.db instance (single-device or NAS-mounted across a household,
-- see QR_NETWORK_STORAGE) -- the shared Persona already has a real row
-- here. This also means no sender_label column is needed (contrast with
-- pending_group_invitations.sender_label, which is deliberately NOT a FK
-- because a group invitation's sender may be on a different install
-- entirely): the owning account is resolvable via
-- personas/user_personas, reusing
-- auth::group_invitations::resolve_persona_owner (already pub(crate)).
--
-- source_persona_display_name: denormalized snapshot at send-time, same
-- reasoning as pending_group_invitations.group_display_name -- a future
-- accept-flow UI (items.id=301+) can identify what's being shared without
-- resolving anything or decrypting the envelope first.
--
-- payload_schema_version: materialization is explicit future work
-- (items.id=301+), so the envelope's plaintext shape must be versioned now
-- rather than discovered as an afterthought once a second version exists.
--
-- encrypted_payload: the ciphertext -- ciphertext = an AEAD envelope
-- (auth::sharing_keypair::encrypt_to_public_key, items.id=289) wrapping the
-- serialized identity+facts snapshot (entities, entity_facts, voice
-- profile entries scoped to this Persona, floor_consent_preference).
-- TEXT, matching this schema family's existing convention (user_salts.
-- salt_hex, pending_group_invitations.encrypted_group_key) of storing
-- encoded ciphertext as TEXT rather than BLOB.
--
-- status / responded_at: exist so a future accept/decline flow
-- (items.id=301+) has somewhere to record the outcome -- no accept/decline
-- logic is implemented by this migration, just the columns. Same
-- 'pending'/'accepted'/'declined' set as pending_group_invitations (this
-- is a join-style accept/decline, not pending_group_key_rotations'
-- 'pending'/'applied' set -- there is no rotation concept here).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS pending_persona_shares (
    id                              TEXT PRIMARY KEY,
    recipient_user_id               TEXT NOT NULL
                                        REFERENCES users(id) ON DELETE CASCADE,
    source_persona_id               TEXT NOT NULL
                                        REFERENCES personas(id) ON DELETE CASCADE,
    source_persona_display_name     TEXT NOT NULL,
    payload_schema_version          TEXT NOT NULL,
    encrypted_payload               TEXT NOT NULL,
    status                          TEXT NOT NULL DEFAULT 'pending'
                                        CHECK (status IN ('pending', 'accepted', 'declined')),
    created_at                      TEXT NOT NULL,
    responded_at                    TEXT,
    extra_metadata                  TEXT NOT NULL DEFAULT '{}'
                                        CHECK (json_valid(extra_metadata))
);

CREATE INDEX IF NOT EXISTS idx_pending_persona_shares_recipient
    ON pending_persona_shares (recipient_user_id, status);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (7, datetime('now'),
    'items.id=299 (narrowed): pending_persona_shares -- shared.db envelope table for SYNCED persona-share grant-flow transport (items.id=189)');
