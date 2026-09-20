-- persistence/schema/keys_002.sql
-- Per-user integration keys database schema: integration_keys.db
-- Migration version: 2
--
-- items.id=528 Phase 2: renames the stored key_type value 'tier2' ->
-- 'qr_hosted', retiring the last piece of internal tier vocabulary this
-- table carried (decisions.id=818). Only the VALUE changes -- the
-- integration_keys table's shape is untouched, so this is a plain UPDATE,
-- not the shim-create/copy/drop recipe a table or column rename would need.
-- keys_001.sql is a v1 file (always re-run on every startup) and its own
-- rerun-safety rule (migrations.rs::validate_v1_file_rerun_safety) forbids
-- a bare UPDATE there outright -- this real, versioned migration is required
-- regardless of whether any real install already has key_type='tier2' rows.
--
-- 'tier3' is NOT touched here: confirmed dead -- zero production Rust code
-- anywhere reads or writes key_type='tier3' (only 'tier2' was ever used,
-- via commands/system.rs and commands/tier2.rs's own TIER2_KEY_TYPE/
-- QR_HOSTED_KEY_TYPE const). keys_001.sql's "Current values" comment is
-- corrected in place (comment-only, safe regardless of applied-version
-- history) to drop the 'tier3' mention along with this rename.

UPDATE integration_keys SET key_type = 'qr_hosted' WHERE key_type = 'tier2';

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (2, datetime('now'), 'items.id=528 Phase 2: key_type "tier2" -> "qr_hosted" (decisions.id=818 vocabulary retirement)');
