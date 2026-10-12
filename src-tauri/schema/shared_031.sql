-- shared_031.sql
--
-- items.id=714 (decisions.id=875 point 7, PROVIDER_REGISTRY_AND_TIER_MODEL_
-- SPEC.md Part 8g; Chat-PM ruling on the items.id=714 plan, 2026-10-12): the
-- Ollama Cloud row gets a pasted API key, not a sidecar sign-in.
--
-- DATA ONLY. No table or column changes, so the schema-shape snapshot is
-- unchanged. Number checked against the live schema directory on the build
-- date (2026-10-12): shared_030 was the latest.
--
-- WHAT CHANGES. providers.ollama_cloud credential_acquisition.method goes
-- from local_signin to guided_paste. keys_page_url is kept. The method is a
-- release-only field (the signed catalog format has no field for it). The
-- sidecar never signs in (QR's sidecar environment is unchanged), so a cloud
-- model can never be reached through the local endpoint by QR's doing; the
-- OllamaClient also refuses any cloud-suffixed model tag as insurance.
--
-- WHAT DOES NOT CHANGE, AND WHY (Chat-PM ruling 4: a key_pattern or a
-- spend_limit_url goes in only where the provider's own official documentation
-- states it, with the source and the date read). Read 2026-10-12 by Code with
-- WebFetch:
--   key format or prefix, NOT STATED on: openrouter.ai/docs/api-reference/
--     authentication; ai.google.dev/gemini-api/docs/api-key; docs.ollama.com/
--     cloud; docs.fireworks.ai/api-reference/introduction; docs.mistral.ai/
--     getting-started/quickstart; docs.deepinfra.com; docs.sambanova.ai/docs/
--     en/get-started/overview; developers.cloudflare.com/workers-ai/platform/
--     pricing; platform.claude.com/docs/en/api/overview (API keys).
--   Groq: console.groq.com/docs/quickstart, /docs/api-keys and /docs/spend-
--     limits returned HTTP 403 to the fetch tool, so nothing was read.
--   spend-limit page, nothing stated that names a limit page for: Gemini (a
--     general billing page only), Ollama (a usage page, not a limit page:
--     ollama.com/settings/usage), Fireworks (quota endpoints only), Mistral (a
--     relative docs link to a workspaces quickstart, not a console page),
--     DeepInfra, SambaNova, Cloudflare (a usage dashboard only).
--   spend-limit page already in the rows and CONFIRMED again: Anthropic
--     (platform.claude.com/settings/billing, "Setting your own spend limit",
--     docs/en/api/rate-limits) and OpenRouter (the keys page, where a credit
--     limit is chosen per key, openrouter.ai/docs/api-reference/authentication).
-- So every key_pattern stays NULL (clipboard assist is inert until a pattern is
-- documented) and the seven missing spend_limit_url values stay NULL (the link
-- is hidden). They are listed for items.id=726. No spend URL changes, so the
-- catalog baseline (catalog/provider-catalog.json) is untouched.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

UPDATE providers
SET credential_acquisition = json_set(credential_acquisition, '$.method', 'guided_paste')
WHERE id = 'ollama_cloud'
  AND json_valid(credential_acquisition)
  AND json_extract(credential_acquisition, '$.method') = 'local_signin';

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (31, datetime('now'),
    'items.id=714: ollama_cloud credential_acquisition.method local_signin to guided_paste (decisions.id=875 point 7 amended by Chat-PM ruling), data only');
