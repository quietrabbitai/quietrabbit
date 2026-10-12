-- shared_032.sql
--
-- items.id=734 (items.id=714 part b; decisions.id=875 point 7, decisions.id=881,
-- PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md Part 8g; Chat-PM review of the
-- items.id=734 plan, 2026-10-12): two row-data fields for the OpenRouter row.
--
-- DATA ONLY. No table or column changes, so the schema-shape snapshot is
-- unchanged. Number checked against the live schema directory on the build
-- date (2026-10-12): shared_031 was the latest.
--
-- credential_acquisition.exchange_url. Where an oauth_pkce provider's
-- authorization code is exchanged for a key. OpenRouter's OAuth page (read by
-- Chat-PM 2026-10-12) documents a POST to its API at /auth/keys with code,
-- code_verifier and code_challenge_method. The path is provider data, so
-- generic code never names it. Like method, it is release-only: the signed
-- catalog format has no field for it and a catalog apply keeps every other key
-- in credential_acquisition as it finds it. Its host must equal the host of
-- the compiled baseline's keys_page_url for the same provider (url_policy).
--
-- credential_acquisition.key_check_path. A path, under the provider's own API
-- base, that needs the key and says nothing else about it: a successful reply
-- means the provider accepts the key. OpenRouter documents GET /key (Bearer)
-- for this. Its model list shows no Authorization requirement, so checking a
-- key against /models would probably let a wrong key pass as verified (not
-- confirmed by a live call: items.id=726). A row without this field is checked
-- against its model list exactly as before.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

UPDATE providers
SET credential_acquisition = json_set(
        credential_acquisition,
        '$.exchange_url', 'https://openrouter.ai/api/v1/auth/keys',
        '$.key_check_path', '/key')
WHERE id = 'openrouter'
  AND json_valid(credential_acquisition)
  AND json_extract(credential_acquisition, '$.method') = 'oauth_pkce';

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (32, datetime('now'),
    'items.id=734: openrouter credential_acquisition gains exchange_url and key_check_path, data only');
