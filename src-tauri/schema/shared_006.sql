-- shared_006.sql
--
-- items.id=288 (266f): key rotation on member departure -- the last of the
-- six items.id=266 group.db sub-items (283/284/285/286/287/289 all
-- complete). GROUP_DB_DESIGN_20260802.md Section 2.5: when a member (or
-- their Persona) leaves or is removed from a group, generate a new group
-- key, redistribute it to remaining members via the same asymmetric-keypair
-- mechanism items.id=284's invitation flow already uses, and re-encrypt
-- group.db under the new key. Section 2.5 is explicit this is NOT fully
-- scoped -- both tables below are this item's own design work, not lifted
-- directly from the source doc.
--
-- Two tables, deliberately kept separate rather than folded into either
-- existing shared.db concept:
--
-- pending_group_key_rotations: same envelope-transport shape as
-- pending_group_invitations (shared_003.sql), same reason for living in
-- shared.db unencrypted (the recipient doesn't have the NEW key yet -- that
-- is precisely what this table exists to deliver). NOT a reuse of
-- pending_group_invitations itself: that table's status CHECK
-- ('pending'/'accepted'/'declined') encodes join semantics, and reusing it
-- here would mean widening a CHECK constraint on a table item 284 already
-- shipped just to bolt on a different lifecycle ('pending'/'applied', no
-- decline -- a rotation isn't optional the way accepting an invitation is).
--
-- group_departures: NOT a durable group-membership table (items.id=290 and
-- items.id=291 stay explicitly out of this item's scope, per Jason's
-- brief). It stores no keys and enumerates only exits, never full
-- membership -- a strictly narrower question than either 290 (durable
-- resident-key storage surviving app restart) or 291 (group creation) would
-- need to answer. Its only job: let remove_member derive "remaining
-- members" as (everyone who ever accepted an invitation to this group) minus
-- (everyone recorded here), without inventing a real membership store.
-- Creator visibility: a group's creator never receives an invitation, so
-- they'd otherwise be invisible to that derivation. items.id=291's
-- auth::group_creation::create_group resolves this without a schema
-- change -- it gives the creator their own pending_group_invitations row,
-- already status='accepted', at creation time (see that module's own
-- header, ROSTER RECONCILIATION).
--
-- group_id: deliberately no FK on either table -- same reasoning
-- shared_003.sql's own header gives: group.db is a separate encrypted file,
-- never tracked as a row in shared.db.
--
-- sender_label (pending_group_key_rotations): matches
-- pending_group_invitations' column name for the same kind of field
-- (who/what initiated this), rather than inventing a differently-named
-- column for an identical purpose.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS pending_group_key_rotations (
    id                      TEXT PRIMARY KEY,
    recipient_persona_id    TEXT NOT NULL REFERENCES personas(id) ON DELETE CASCADE,
    group_id                TEXT NOT NULL,
    encrypted_group_key     TEXT NOT NULL,
    sender_label            TEXT NOT NULL,
    status                  TEXT NOT NULL DEFAULT 'pending'
                                CHECK (status IN ('pending', 'applied')),
    created_at              TEXT NOT NULL,
    applied_at              TEXT,
    extra_metadata          TEXT NOT NULL DEFAULT '{}'
                                CHECK (json_valid(extra_metadata))
);

CREATE INDEX IF NOT EXISTS idx_pending_group_key_rotations_recipient
    ON pending_group_key_rotations (recipient_persona_id, status);

CREATE TABLE IF NOT EXISTS group_departures (
    group_id        TEXT NOT NULL,
    persona_id      TEXT NOT NULL,
    departed_at     TEXT NOT NULL,
    reason          TEXT NOT NULL CHECK (reason IN ('left', 'removed')),
    PRIMARY KEY (group_id, persona_id)
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (6, datetime('now'),
    'items.id=288 (266f): pending_group_key_rotations + group_departures -- key rotation on member departure');
