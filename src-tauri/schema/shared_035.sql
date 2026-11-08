-- shared_035.sql
--
-- items.id=873 (decisions.id=934): declared hosts for the Cloud Chat pane
-- navigation policy. Number checked against the live schema directory on the
-- build date (2026-11-08): shared_034 was the latest. The table is `providers`:
-- tier3_providers was generalised into it by shared_013.sql and stays gone.
--
-- navigate_hosts. JSON array of plain host names the pane may load as a top-level
-- navigation (sign-in hosts included). The host of the row's own launch_url is
-- always allowed in addition, so an empty array means launch host only.
-- Matching is exact after normalisation (lowercase, IDNA ASCII, no trailing dot).
-- An entry that is not a plain host (a wildcard, a scheme, a port, an IP literal,
-- an empty string) declares nothing, and so does a value that is not an array of
-- strings: the Rust reader fails closed (cloud_chat_gpu_pane/nav_policy.rs).
--
-- page_traffic_hosts. JSON array of hosts the provider's page is known to contact
-- by itself (analytics and similar). Declared, never blocked, and NOT enforced
-- anywhere yet: its first consumer is the idle-egress assertion of items.id=619.
--
-- RELEASE-ONLY. Both columns are outside the signed catalog's governed columns
-- (provider_catalog/apply.rs fingerprints every other column), by decision: a host
-- widens what a pane may load. A correction after measuring a real login is a new
-- numbered migration holding data only (UPDATE providers SET navigate_hosts = ...),
-- shipped in a release, or a hand edit of shared.db in a dev or scratch profile.
-- A running pane keeps the host set it opened with: a change takes effect when
-- that provider's pane is next opened.
--
-- Seeds. duckai: duck.ai and improving.duckduckgo.com (items.id=870 capture, logged
-- out). gemini: gemini.google.com and accounts.google.com (items.id=853 and 849
-- measurements). claude and chatgpt: launch host only, UNMEASURED until a real login
-- is captured with Jason present, so their sign-in hosts (social login, SSO) raise
-- the confirm until a later migration declares them.
--
-- user_privacy_summary. One placeholder sentence is appended to the summary of the
-- four rows (the page may send telemetry to its own hosts, decisions.id=934 point 3).
-- Existing wording is kept. The field is stored but nothing displays it yet, and
-- CloudChatProviderSummary omits it. Wording is Chat-BRAND copy.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals or comments -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE providers ADD COLUMN navigate_hosts TEXT NOT NULL DEFAULT '[]'
    CHECK (json_valid(navigate_hosts));
ALTER TABLE providers ADD COLUMN page_traffic_hosts TEXT NOT NULL DEFAULT '[]'
    CHECK (json_valid(page_traffic_hosts));

UPDATE providers
SET navigate_hosts = '["duck.ai"]',
    page_traffic_hosts = '["improving.duckduckgo.com"]'
WHERE id = 'duckai';

UPDATE providers
SET navigate_hosts = '["gemini.google.com","accounts.google.com"]'
WHERE id = 'gemini';

UPDATE providers
SET navigate_hosts = '["claude.ai"]'
WHERE id = 'claude';

UPDATE providers
SET navigate_hosts = '["chatgpt.com"]'
WHERE id = 'chatgpt';

UPDATE providers
SET user_privacy_summary = json_set(
        user_privacy_summary,
        '$.summary',
        json_extract(user_privacy_summary, '$.summary')
            || ' [Placeholder] The provider page may also send telemetry to its own hosts.')
WHERE id IN ('duckai', 'claude', 'chatgpt', 'gemini')
  AND user_privacy_summary IS NOT NULL
  AND json_valid(user_privacy_summary)
  AND json_extract(user_privacy_summary, '$.summary') IS NOT NULL;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (35, datetime('now'),
    'items.id=873: providers.navigate_hosts and page_traffic_hosts -- declared hosts for the Cloud Chat pane navigation policy (decisions.id=934), seeded for the four embedded rows, plus a telemetry placeholder sentence in their user_privacy_summary');
