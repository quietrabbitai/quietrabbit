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
--      bookkeeping. Modeled directly on group_sync_settings (shared_005.sql)
--      -- same placement reasoning (unencrypted, so a periodic sweep can
--      find the configured path before it knows whether it even has a key
--      to use it with), same upsert-on-reconfigure semantics, same
--      "None/no row = not configured yet, a silent no-op" contract.
--
--      UNLIKE group_sync_settings, this table carries an explicit `role`
--      column. Group folder-sync is symmetric -- every member with a key
--      polls the same shared documents/ directory the same way. Persona
--      sharing is directional: for a given share_id, the owner's row is a
--      PUSH source (their own source_persona_id) and the recipient's row is
--      a PULL sink (their own materialized_persona_id) -- never both for the
--      same person. Making that explicit avoids inferring role by
--      cross-referencing pending_persona_shares on every sweep.
--
--      last_synced_at / last_pushed_at / last_content_hash are three
--      separate columns rather than reusing group_sync_settings' single
--      last_synced_at: an owner row only ever populates last_pushed_at/
--      last_content_hash (push-side bookkeeping, the content hash lets a
--      periodic re-push skip a write when nothing changed -- Jason's
--      direction, narrowing decisions.id=722's "push-on-save" to a periodic
--      re-push given persona content has no single save-hook choke point
--      the way group.db's one-document-one-save-function shape does); a
--      recipient row only ever populates last_synced_at (pull-side). Both
--      columns exist on every row rather than splitting into two tables so
--      one settings_store module can serve both roles uniformly.

ALTER TABLE pending_persona_shares ADD COLUMN materialized_persona_id TEXT
    REFERENCES personas(id) ON DELETE SET NULL;

CREATE TABLE IF NOT EXISTS persona_share_sync_settings (
    persona_id          TEXT NOT NULL,
    share_id            TEXT NOT NULL,
    role                TEXT NOT NULL CHECK (role IN ('owner', 'recipient')),
    folder_path         TEXT NOT NULL,
    last_synced_at      TEXT,
    last_pushed_at      TEXT,
    last_content_hash   TEXT,
    last_error          TEXT,
    updated_at          TEXT NOT NULL,
    PRIMARY KEY (persona_id, share_id)
);
