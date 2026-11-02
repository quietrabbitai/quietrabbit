-- shared_005.sql
--
-- items.id=287 (group.db 266e): folder-sync push/pull
-- (Working/GROUP_DB_DESIGN_20260802.md Section 2.4) added group_sync_settings
-- here: one row per (persona_id, group_id) holding this install's
-- folder-sync destination and the outcome of the last push or pull.
--
-- items.id=661 (decisions.id=862) moved that table out of the plaintext
-- shared.db into the user's encrypted integration_keys.db (keys_006.sql). The
-- folder path is a user-chosen filesystem location and last_error can embed
-- one, and shared.db is readable without the password. The original reason for
-- keeping it here, that the periodic pull must find the path before it knows
-- whether it has a key, does not hold: the pull only runs for group keys that
-- are resident, which exist only while the account is unlocked.
--
-- This file is edited in place, which decisions.id=857 allows until first
-- ship. It keeps its version number (versions must stay contiguous) and now
-- only records that version.

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (5, datetime('now'),
    'items.id=287: folder-sync settings for group.db (table since moved to the encrypted integration_keys.db by items.id=661, keys_006.sql)');
