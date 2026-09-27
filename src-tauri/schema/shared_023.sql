-- shared_023.sql
--
-- items.id=435: hardware-capability-detection module. Two new
-- instance_config rows (shared_001.sql's existing generic key/value table)
-- caching the detected HardwareProfile (src-tauri/src/hardware_probe.rs) --
-- same "config as data, not a new table" idiom shared_022.sql's
-- nightly_batch_hour/nightly_batch_last_run_at rows already establish.
--
-- hardware_profile_json: serialized HardwareProfile (JSON), or '' meaning
-- "never detected" -- same empty-string sentinel instance_name/
-- nightly_batch_last_run_at already use. Detected once and cached: unlike
-- Ollama's live-every-launch re-probe (D6-353, ollama_sidecar.rs), this
-- machine's RAM/CPU/GPU class doesn't change between launches, so
-- get_or_detect() only re-probes on a cache miss. Re-detection is otherwise
-- a deliberate, later-triggered action (e.g. a future onboarding
-- "re-detect hardware" affordance, items.id=437), not automatic.
--
-- hardware_profile_detected_at: RFC3339 UTC timestamp of the cached
-- detection, or '' meaning "never run" -- same sentinel.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

INSERT OR IGNORE INTO instance_config VALUES ('hardware_profile_json', '');
INSERT OR IGNORE INTO instance_config VALUES ('hardware_profile_detected_at', '');

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (23, datetime('now'),
    'items.id=435: instance_config rows for cached hardware-capability probe (hardware_profile_json, hardware_profile_detected_at)');
