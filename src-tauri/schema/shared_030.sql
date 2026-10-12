-- shared_030.sql
--
-- items.id=713 (decisions.id=875 point 6, PROVIDER_REGISTRY_AND_TIER_MODEL_
-- SPEC.md Part 8f): the stored state of the signed provider catalog update.
-- Number checked against the live schema directory on the build date
-- (2026-10-11): shared_029 was the latest. items.id=726 may add a later file.
--
-- WHAT LIVES HERE. Provider and model rows stay in providers and
-- provider_models (global, plaintext, curator-owned). A catalog update, applied
-- only by a user click, rewrites a short list of neutral columns on those rows
-- (prices, dates, flags, limits) and nothing else. These two tables record
-- which catalog is in force and what is needed to undo it.
--
-- provider_catalog_state: exactly one row (id = 1).
--   applied_version  the catalog_version in force, 0 = none applied (the rows
--                    are as the release migrations left them).
--   applied_sha256   SHA-256 (hex) of the applied catalog document.
--   declined_version the highest version the user rolled back. A catalog at or
--                    below it is not offered again.
--
-- provider_catalog_history: one row per applied catalog.
--   document         the applied catalog text, verbatim (kept so a later
--                    release can re-apply it, see provider_catalog::reconcile).
--   undo             before-images of exactly the columns the apply touched,
--                    plus the ids of models it added. Rolling back restores
--                    these. Only the latest non-rolled-back row can be rolled
--                    back, and only the last two rows are kept.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS provider_catalog_state (
    id               INTEGER PRIMARY KEY CHECK (id = 1),
    applied_version  INTEGER NOT NULL DEFAULT 0 CHECK (applied_version >= 0),
    applied_sha256   TEXT,
    applied_at       TEXT,
    declined_version INTEGER NOT NULL DEFAULT 0 CHECK (declined_version >= 0)
);

INSERT OR IGNORE INTO provider_catalog_state (id) VALUES (1);

CREATE TABLE IF NOT EXISTS provider_catalog_history (
    version        INTEGER PRIMARY KEY CHECK (version > 0),
    sha256         TEXT NOT NULL,
    document       TEXT NOT NULL CHECK (json_valid(document)),
    undo           TEXT NOT NULL CHECK (json_valid(undo)),
    applied_at     TEXT NOT NULL,
    rolled_back_at TEXT
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (30, datetime('now'),
    'items.id=713: provider_catalog_state (the catalog version in force) and provider_catalog_history (applied catalog documents with before-images for rollback), decisions.id=875 point 6');
