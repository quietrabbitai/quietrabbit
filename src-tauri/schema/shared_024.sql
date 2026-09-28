-- shared_024.sql
--
-- items.id=436: local Ollama model install mechanism. Adds install-state
-- and purpose-flag columns to `providers` and seeds the curated Release 1
-- local-model catalog as one row per model (spec doc §5c's recommended
-- default, decided for this item per decisions.id=840's dedicated-sidecar
-- architecture).
--
-- `installed` / `qr_disabled_by_user`: install-mechanism-owned state,
-- mutated only via provider_store::set_local_model_installed/
-- set_local_model_uninstalled/set_local_model_disabled -- a narrow,
-- explicit exception to this table's normal "release-time seeding only,
-- no IPC-exposed writes" rule (see provider_store.rs module header).
-- Everything else about these rows (hardware_requirement, risk_rating,
-- documentation_gate, etc.) stays curator-owned, unchanged by that
-- exception.
--
-- `local_model_tag`: the literal string Ollama's API expects (e.g.
-- "llama3.2:3b"). `providers` has no other "model tag" column -- cloud
-- providers store per-model detail in the separate `provider_models`
-- table, but local models get their own top-level row per spec §5c, so
-- the tag has to live here.
--
-- `local_model_digest` / `installed_at`: populated by
-- set_local_model_installed() on a successful pull; cleared by
-- set_local_model_uninstalled() on delete. Deliberately placed on
-- `providers` rather than `user_provider_preference` (where an earlier
-- spec-doc sketch, Part 2b, had put similarly-named columns) -- whether a
-- model is installed is a property of the model row itself, independent
-- of any per-user preference.
--
-- `focus_eligible` / `cloud_chat_visible`: sibling purpose flags to the
-- existing `qr_internal_eligible` (added shared_013.sql) -- "available for
-- Focuses" / "visible in Cloud Chat" / "internal QR use" from this item's
-- dispatch. Same unmarked-is-exclusion semantics as qr_internal_eligible.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE providers ADD COLUMN installed INTEGER NOT NULL DEFAULT 0
    CHECK (installed IN (0, 1));
ALTER TABLE providers ADD COLUMN qr_disabled_by_user INTEGER NOT NULL DEFAULT 0
    CHECK (qr_disabled_by_user IN (0, 1));
ALTER TABLE providers ADD COLUMN local_model_tag TEXT;
ALTER TABLE providers ADD COLUMN local_model_digest TEXT;
ALTER TABLE providers ADD COLUMN installed_at TEXT;
ALTER TABLE providers ADD COLUMN focus_eligible INTEGER NOT NULL DEFAULT 0
    CHECK (focus_eligible IN (0, 1));
ALTER TABLE providers ADD COLUMN cloud_chat_visible INTEGER NOT NULL DEFAULT 0
    CHECK (cloud_chat_visible IN (0, 1));

-- Curated local-model catalog, Release 1. Kept in sync by hand with
-- providers::evaluation::RELEASE_1_MODELS -- if that list changes, a
-- later migration must add/deprecate rows to match. `installed` starts at
-- its DEFAULT 0 for all three -- the catalog entry existing is distinct
-- from the model actually being downloaded (that's the whole point of
-- this column). hardware_requirement values are rough sizing from known
-- quantized weight sizes, not a benchmark -- good enough to not leave the
-- column empty; min_ram_class values match hardware_probe::RamClass's
-- exact snake_case variants (low/medium/high/very_high) so a future
-- hardware-matching pass can consume them without translation.
INSERT OR IGNORE INTO providers (
    id, display_name, provider_type, mode, launch_url, activation_status,
    documentation_gate, created_at, is_local, is_anonymous, retains_data,
    trains_on_data_by_default, login_required, qr_internal_eligible,
    privacy_guardian_default_level, risk_rating, hardware_requirement,
    preference_tier, local_model_tag, focus_eligible, cloud_chat_visible
) VALUES
    ('ollama:llama3.2:3b', 'Llama 3.2 (3B)', 'local_model', 'local', NULL,
     'active', '{}', datetime('now'), 1, 1, 0, 0, 0, 1, 'low', 1,
     '{"min_ram_class":"low"}', 'preferred', 'llama3.2:3b', 1, 1),
    ('ollama:llama3.1:8b', 'Llama 3.1 (8B)', 'local_model', 'local', NULL,
     'active', '{}', datetime('now'), 1, 1, 0, 0, 0, 1, 'low', 1,
     '{"min_ram_class":"medium"}', 'preferred', 'llama3.1:8b', 1, 1),
    ('ollama:qwen2.5:7b', 'Qwen 2.5 (7B)', 'local_model', 'local', NULL,
     'active', '{}', datetime('now'), 1, 1, 0, 0, 0, 1, 'low', 1,
     '{"min_ram_class":"medium"}', 'preferred', 'qwen2.5:7b', 1, 1);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (24, datetime('now'),
    'items.id=436: providers gains installed/qr_disabled_by_user/local_model_tag/local_model_digest/installed_at/focus_eligible/cloud_chat_visible columns; seeds Release 1 curated local-model catalog (llama3.2:3b, llama3.1:8b, qwen2.5:7b) as one providers row per model');
