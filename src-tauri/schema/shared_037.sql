-- shared_037.sql
--
-- items.id=873 (decisions.id=934 addendum 2, 2026-11-08): login hosts on the providers
-- row. Number checked against the live schema directory on the build date: shared_036
-- was the latest (it dropped the earlier navigate_hosts and page_traffic_hosts).
--
-- login_hosts. JSON array of plain host names. For a row that declares any, a
-- main-frame navigation of its Cloud Chat pane to a host outside the list loads the
-- row's launch_url instead, and the launch host is always in the list. A row with
-- an empty array declares none and is unrestricted (its login set is open: Google,
-- Apple, phone, enterprise SSO). Matching is exact host, no wildcard: an entry that
-- is not a plain host declares nothing, and a value that is not an array of strings
-- declares nothing, so the row is unrestricted (fail open,
-- cloud_chat_gpu_pane/nav_hosts.rs). A running pane keeps the list it opened with.
--
-- RELEASE-ONLY, as the earlier host columns were: outside the signed catalog's
-- governed columns, because a host list widens or narrows what a pane may load. A
-- correction is a new numbered data-only migration, or a hand edit of shared.db in a
-- dev or scratch profile.
--
-- Seed, Gemini only, taken from Google's published documentation, not from a capture:
--   * gemini.google.com, accounts.google.com and accounts.youtube.com: the Google
--     Workspace Help page "Set up a Google Workspace host name allowlist" (last
--     updated 2026-10-26) lists gemini.google.com under Generative AI,
--     accounts.google.com under Authentication, and accounts.youtube.com under its
--     optional "More authentication URLs".
--   * accounts.google.co.<cc>: the same page names the form
--     accounts.google.co.[your country identifier] (example accounts.google.co.jp).
--     The 24 country codes are the .google.co.<cc> entries of Google's published
--     list https://www.google.com/supported_domains (187 entries, fetched 2026-11-08):
--     ao bw ck cr id il in jp ke kr ls ma mz nz th tz ug uk uz ve vi za zm zw.
--   Not seeded, and why:
--     * www.google.com: the page DOES list https://www.google.com and
--       https://www.google.com/accounts/ among its authentication URLs, in the same
--       optional list that carries accounts.youtube.com. It is left out on purpose:
--       it also serves Google Search, and matching is by host only, so listing it
--       would let the pane load Search and its result links. Google's robot check
--       (www.google.com/sorry, status 429) is on that host and passes through the
--       one-bounce fail-open instead (cloud_chat_gpu_pane/nav_hosts.rs).
--     * myaccount.google.com and the apps-menu hosts: not listed by the page as
--       sign-in hosts.
--     * the .google.com.<cc> and bare country forms: the page documents only
--       accounts.google.co.<cc>.
--   A user sent to an unlisted host bounces to Gemini until a data update lists it.
-- No other row declares hosts, so duckai, claude and chatgpt stay unrestricted.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals or comments -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE providers ADD COLUMN login_hosts TEXT NOT NULL DEFAULT '[]'
    CHECK (json_valid(login_hosts));

UPDATE providers
SET login_hosts = '["gemini.google.com","accounts.google.com","accounts.youtube.com","accounts.google.co.ao","accounts.google.co.bw","accounts.google.co.ck","accounts.google.co.cr","accounts.google.co.id","accounts.google.co.il","accounts.google.co.in","accounts.google.co.jp","accounts.google.co.ke","accounts.google.co.kr","accounts.google.co.ls","accounts.google.co.ma","accounts.google.co.mz","accounts.google.co.nz","accounts.google.co.th","accounts.google.co.tz","accounts.google.co.ug","accounts.google.co.uk","accounts.google.co.uz","accounts.google.co.ve","accounts.google.co.vi","accounts.google.co.za","accounts.google.co.zm","accounts.google.co.zw"]'
WHERE id = 'gemini';

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (37, datetime('now'),
    'items.id=873: providers.login_hosts -- declared login hosts for the Cloud Chat pane navigation scope (decisions.id=934 addendum 2), seeded for Gemini only');
