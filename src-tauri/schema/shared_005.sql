-- shared_005.sql
--
-- items.id=287 (group.db 266e): folder-sync push/pull
-- (Working/GROUP_DB_DESIGN_20260802.md Section 2.4). One row per
-- (persona_id, group_id): where this install's folder-sync destination for
-- that group is, and the outcome of the most recent push/pull attempt.
--
-- WHY shared.db, NOT group.db: same reasoning as pending_group_invitations
-- (shared_003.sql) -- the folder path is not secret (it's a filesystem
-- location, not group content) and must be readable independent of the
-- group key being unlocked -- the periodic pull loop needs to know *where*
-- to look before it can decide whether it even has a key to look with.
--
-- WHY A NEW FILE, NOT edited into shared_004.sql: same reasoning
-- shared_002/003/004.sql each already give -- a genuinely separate,
-- independently-shipped feature landing after them. Continues the existing
-- version sequence; no prior file touched.
--
-- folder_path: the configured OS-writable folder for this group, this
-- install (design doc Section 2.4: local share, NAS, or a cloud-sync
-- client's local mount -- QR never integrates with a cloud provider
-- directly, it only ever writes to a plain filesystem path). Genuinely
-- per-(persona_id, group_id): even though conceptually "the group's"
-- folder, the actual local mount point can differ per install (e.g. two
-- members' cloud-sync clients mount the same logical folder at different
-- local paths), so there is one row per install's own configuration, not
-- one shared value.
--
-- last_synced_at / last_error: outcome of the most recent push OR pull
-- attempt for this (persona_id, group_id) pair. NULL/NULL until the first
-- attempt. Failure handling is fail-silent-retry-next-cycle (confirmed with
-- Jason, items.id=287 planning) -- these two columns exist so a future UI
-- can surface sync health without this item needing to build that UI now.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS group_sync_settings (
    persona_id      TEXT NOT NULL,
    group_id        TEXT NOT NULL,
    folder_path     TEXT NOT NULL,
    last_synced_at  TEXT,
    last_error      TEXT,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (persona_id, group_id)
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (5, datetime('now'),
    'items.id=287: group_sync_settings -- shared.db per-(persona_id,group_id) folder-sync path + last push/pull outcome');
