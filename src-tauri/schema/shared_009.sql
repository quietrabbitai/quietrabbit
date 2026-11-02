-- shared_009.sql
--
-- items.id=303: ongoing sync transport for SYNCED persona sharing.
-- decisions.id=722. Builds on items.id=302's materialization and
-- items.id=299's grant-flow envelope mechanism -- no new crypto primitive,
-- no server component (decisions.id=722).
--
-- TWO CHANGES:
--   1. pending_persona_shares.materialized_persona_id -- additive column.
--      accept_persona_share (items.id=302) generates a brand-new persona_id
--      at accept time but never wrote it back onto the share row itself; the
--      only place it was ever surfaced was the function's own return value.
--      The sync engine needs a durable way to answer "which persona did
--      share X materialize into" across process restarts, without depending
--      on whatever originally called accept_persona_share to have recorded
--      it somewhere else. Nullable: NULL for any share not yet accepted, or
--      accepted before this column existed (none in production -- pre-
--      release -- but nullable is the honest shape regardless).
--
--   2. persona_share_sync_settings -- this install's folder-sync
--      destination for one (persona_id, share_id) pair, plus push/pull
--      bookkeeping, with an explicit `role` column (owner pushes, recipient
--      pulls). items.id=661 (decisions.id=862) moved this table out of the
--      plaintext shared.db into the user's encrypted integration_keys.db
--      (keys_006.sql): the folder path is a user-chosen location and
--      last_error can embed one. This file is edited in place, which
--      decisions.id=857 allows until first ship.

ALTER TABLE pending_persona_shares ADD COLUMN materialized_persona_id TEXT
    REFERENCES personas(id) ON DELETE SET NULL;
