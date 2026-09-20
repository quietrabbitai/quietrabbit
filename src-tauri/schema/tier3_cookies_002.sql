-- persistence/schema/tier3_cookies_002.sql
-- Per-user Tier 2/3 provider cookie database schema: tier3_cookies.db
-- Migration version: 2
--
-- items.id=528 Phase 2: completes the tier3_provider_cookies ->
-- cloud_chat_provider_cookies rename (decisions.id=818 vocabulary
-- retirement) that tier3_cookies_001.sql's own amendment started.
--
-- WHY THIS FILE EXISTS SEPARATELY FROM THE v1 AMENDMENT: tier3_cookies_001.sql
-- is v1, and v1 files re-run on EVERY startup (persistence/migrations.rs's
-- own rule) -- so simply amending v1 to create the new-named table would
-- leave any existing install's real data stranded under the OLD name
-- forever (v1 only ever creates the NEW name now; it never touches an
-- already-existing old-named table). This migration is the one-time bridge:
--
--   1. Shim: recreate the OLD table if it doesn't already exist (a fresh
--      install's v1 run never created it, since v1 now only creates the new
--      name -- this INSERT...SELECT below needs a real table to select from
--      either way, even if it selects zero rows).
--   2. Copy: move any real rows from the old name to the new name.
--   3. Drop: remove the old table (and its index, dropped automatically)
--      once its data is safely copied across.
--
-- Fresh DB: v1 creates cloud_chat_provider_cookies empty; step 1's shim
-- creates an empty tier3_provider_cookies; step 2 copies 0 rows; step 3
-- drops the empty shim. End state: only the new table, empty. Correct.
--
-- Existing DB (real cookies under the old name): v1 re-run creates
-- cloud_chat_provider_cookies empty (first time); step 1's shim CREATE is a
-- no-op (the real old table already exists with data); step 2 copies the
-- real rows across; step 3 drops the now-empty old table. End state: the
-- new table has the migrated data. Correct.

CREATE TABLE IF NOT EXISTS tier3_provider_cookies (
    id              TEXT PRIMARY KEY,
    provider_id     TEXT NOT NULL,
    name            TEXT NOT NULL,
    value           TEXT NOT NULL,
    domain          TEXT NOT NULL,
    path            TEXT NOT NULL,
    secure          INTEGER NOT NULL DEFAULT 0,
    httponly        INTEGER NOT NULL DEFAULT 0,
    same_site       INTEGER NOT NULL DEFAULT 0,
    priority        INTEGER NOT NULL DEFAULT 0,
    has_expires     INTEGER NOT NULL DEFAULT 0,
    expires         INTEGER,
    creation        INTEGER NOT NULL,
    last_access     INTEGER NOT NULL,
    updated_at      TEXT NOT NULL,
    UNIQUE (provider_id, domain, path, name)
);

INSERT OR IGNORE INTO cloud_chat_provider_cookies
    (id, provider_id, name, value, domain, path, secure, httponly, same_site,
     priority, has_expires, expires, creation, last_access, updated_at)
SELECT id, provider_id, name, value, domain, path, secure, httponly, same_site,
       priority, has_expires, expires, creation, last_access, updated_at
FROM tier3_provider_cookies;

DROP TABLE tier3_provider_cookies;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (2, datetime('now'), 'items.id=528 Phase 2: tier3_provider_cookies -> cloud_chat_provider_cookies (decisions.id=818 vocabulary retirement)');
