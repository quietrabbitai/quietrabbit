-- shared_036.sql
--
-- items.id=873 (rescoped 2026-11-08, decisions.id=934 addendum): the declared-hosts
-- navigation gate is withdrawn, so the two columns shared_035.sql added to
-- providers have no reader. They are dropped here. shared_035.sql stays as it is,
-- because it was pushed and a developer database already records version 35.
-- A database that never saw version 35 runs both files and ends without the columns.
--
-- Dropped: providers.navigate_hosts and providers.page_traffic_hosts. Each carries
-- its own single-column CHECK (json_valid), which SQLite drops with the column, so
-- no table rebuild is needed (same technique as shared_015.sql and messages_005.sql).
-- Nothing else in the schema references either column.
--
-- Kept: the placeholder sentence shared_035.sql appended to user_privacy_summary of
-- the four embedded rows. Provider analytics stays declared and not blocked
-- (decisions.id=934, still in force), and items.id=874 and 875 own the wording and
-- the display.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals or comments -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE providers DROP COLUMN navigate_hosts;
ALTER TABLE providers DROP COLUMN page_traffic_hosts;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (36, datetime('now'),
    'items.id=873: drop providers.navigate_hosts and page_traffic_hosts (the declared-hosts navigation gate was withdrawn, decisions.id=934 addendum 2026-11-08), the user_privacy_summary placeholder sentence is kept');
