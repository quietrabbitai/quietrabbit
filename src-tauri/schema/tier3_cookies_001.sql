-- persistence/schema/tier3_cookies_001.sql
-- Per-user Tier 2/3 provider cookie database schema: tier3_cookies.db
-- Encrypted with SQLCipher using user master key.
-- Path: /users/{user_id}/tier3_cookies.db
-- Migration version: 1
--
-- items.id=224 resolution, decisions.id=711 (2026-08-04): CEF's Chrome-
-- runtime ChromeBrowserContext structurally rejects any second
-- RequestContext (diagnosed via src-tauri/examples/repro_224.rs) -- the
-- per-pane contexts/<provider_id> RequestContext approach in
-- tier3_pane/sync_window.rs never worked. The fix keeps CEF's one working
-- global RequestContext (one shared cookie jar, domain-scoped the same way
-- any ordinary browser profile already keeps different sites' cookies
-- apart) and moves PERSISTENCE across app restarts into QR's own storage
-- instead. This table is that storage: the jar is the live working copy
-- during a session, this table is the source of truth across restarts.
--
-- Per-user, not per-persona (mirrors keys_001.sql/integration_keys.db's
-- own scoping, not personal.db's) -- decisions.id=711's extended_notes
-- name the key explicitly as "(user, provider)". auth::registry::KeyRegistry
-- only ever holds user_id (no resident persona_id), matching this. The
-- "user" half of that key is the DB file itself (one tier3_cookies.db per
-- user, same as integration_keys.db) -- no user_id column, matching
-- integration_keys_001.sql's own convention of not duplicating the file's
-- own scope as a row value.
--
-- ROW SHAPE: one row per RFC6265 cookie identity (domain, path, name),
-- scoped further by provider_id -- a domain overlap between two different
-- providers (unlikely, but not schema-prevented) must not collide.
-- UNIQUE (provider_id, domain, path, name) is exactly that identity tuple.
--
-- CEF Cookie struct field mapping (cef::Cookie, vendored cef crate
-- v151.1.0+151.3.12, confirmed against the actual Rust bindings, not
-- assumed from the C++ API): secure/httponly/has_expires are CEF's own
-- c_int booleans, stored here as INTEGER 0/1 same as this codebase's other
-- boolean columns (e.g. tier3_providers.login_required). same_site/priority
-- are CEF's own C enums (CookieSameSite/CookiePriority) -- stored as their
-- raw i32 value via get_raw(), round-tripped back through the matching
-- From impl at read time, never reinterpreted by this schema. creation/
-- last_access/expires are CEF's Basetime { val: i64 } -- an opaque internal
-- time value, stored and restored verbatim as INTEGER, no epoch conversion
-- performed by this codebase (CEF owns that interpretation entirely).
-- expires/creation/last_access are nullable together only insofar as
-- has_expires governs whether expires is meaningful (has_expires=0 ->
-- expires is CEF's zeroed Basetime default, not a real value -- callers
-- must check has_expires, not expires IS NULL, exactly mirroring
-- cef::Cookie's own has_expires-gates-expires contract).
--
-- No REFERENCES tier3_providers(id) -- tier3_providers lives in shared.db,
-- a separate physical SQLite file from tier3_cookies.db. SQLite has no
-- cross-database foreign key support (same reasoning keys_001.sql's own
-- header already gives for dropping persona_id's FK -- see that file).
-- provider_id validity against the live catalog is an application-layer
-- concern (commands/tier3_pane.rs already resolves provider_id ->
-- provider via provider_store::get_provider before this table is ever
-- touched).

-- Required by every version-1 schema file in this codebase -- the
-- migration runner (persistence/migrations.rs::run_pending) records
-- applied versions into this table itself but does not create it; each
-- v1 schema file owns that (matching keys_001.sql/personal_001.sql's own
-- schema_version block verbatim).
CREATE TABLE IF NOT EXISTS schema_version (
    version         INTEGER PRIMARY KEY,
    applied_at      TEXT NOT NULL,
    description     TEXT NOT NULL
);

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

CREATE INDEX IF NOT EXISTS idx_tier3_provider_cookies_lookup
    ON tier3_provider_cookies (provider_id);
