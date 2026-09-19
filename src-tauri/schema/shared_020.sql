-- shared_020.sql
--
-- items.id=529 -- retires the numeric 1/2/3 legacy-tier encoding for
-- ExternalAccess-typed columns in favor of storing ExternalAccess::as_str()
-- values directly, closing the gap that made AnonymousPreferred
-- ("anonymous provider preferred, full-account provider an acceptable
-- fallback" -- Jason, 2026-09-19) structurally unrepresentable: no INTEGER
-- BETWEEN 1 AND 3 column has a 4th slot for it, and the only way to add one
-- while staying numeric would require renumbering Unrestricted (see below),
-- itself a real migration. Storing the enum's own string form instead needs
-- no renumbering and is self-documenting in the raw DB.
--
-- WHY NOT JUST WIDEN THE CHECK TO BETWEEN 1 AND 4 (numeric renumbering):
-- execution_tier's Axis-1 ceiling calc (lifecycle.rs execute_step()) took
-- the raw numeric MIN across routing_tier/max_routing_tier/max_permitted_tier
-- u8 values -- correctness there depended on the legacy numbers' ordering
-- matching ExternalAccess's real ordering (LocalOnly < AnonymousRequired <
-- AnonymousPreferred < Unrestricted). Slotting AnonymousPreferred in at 4
-- would sit numerically ABOVE Unrestricted's 3, backwards from its real
-- position, corrupting that min() wherever a step/Focus actually used it.
-- Preserving correct ordering numerically would mean renumbering
-- Unrestricted 3->4 instead -- a value migration touching just as much
-- surface as this file, with none of the self-documentation benefit and a
-- lossy from_legacy_tier()/as_legacy_tier() round-trip (AnonymousPreferred
-- had no legacy slot of its own) left permanently in place. Decision 1
-- (Jason, 2026-09-19): string-native storage, closed CHECK IN (...) for the
-- same enforcement the INTEGER CHECK gave, applied to all 3 locations below.
--
-- WHY A TABLE REBUILD, NOT ALTER TABLE ADD COLUMN / ALTER COLUMN: SQLite has
-- no ALTER COLUMN to change a column's type or swap its CHECK constraint --
-- same constraint shared_011.sql hit rebuilding this exact
-- focus_settings_friction_decisions table. Runs inside run_pending's
-- per-version SAVEPOINT (migrations.rs), atomic with the rest of this file.
--
-- MIGRATION APPROACH FOR EXISTING ROWS: value-mapped copy (1->'local_only',
-- 2->'anonymous_required', 3->'unrestricted'), not a destructive drop.
-- Pre-release/testing-only data would have tolerated a destructive
-- migration, but the mapping is exact and trivial (every legacy value ever
-- written is provably 1, 2, or 3 -- AnonymousPreferred could not be
-- authored or stored before this file), so there is no reason to discard
-- real rows when preserving them costs nothing extra. NULL requested_*/
-- existing_* values (shared_011.sql's own NULLable columns) stay NULL.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

-- ---------------------------------------------------------------------------
-- focus_settings.max_permitted_tier: INTEGER -> TEXT
-- ---------------------------------------------------------------------------

CREATE TABLE focus_settings_new (
    persona_id          TEXT NOT NULL REFERENCES personas(id) ON DELETE CASCADE,
    focus_id            TEXT NOT NULL,
    context_flow        TEXT NOT NULL DEFAULT 'bidirectional'
                            CHECK (context_flow IN (
                                'bidirectional', 'receive_only', 'isolated'
                            )),
    library_visibility  TEXT NOT NULL DEFAULT 'shared'
                            CHECK (library_visibility IN (
                                'shared', 'persona_visible', 'persona_hidden'
                            )),
    privacy_tier        INTEGER NOT NULL DEFAULT 2
                            CHECK (privacy_tier BETWEEN 1 AND 3),
    max_permitted_tier  TEXT NOT NULL DEFAULT 'anonymous_required'
                            CHECK (max_permitted_tier IN (
                                'local_only', 'anonymous_required',
                                'anonymous_preferred', 'unrestricted'
                            )),
    focus_profile       TEXT NOT NULL DEFAULT 'open'
                            CHECK (focus_profile IN (
                                'open', 'organized', 'protected'
                            )),
    voice_override      TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    PRIMARY KEY (persona_id, focus_id)
);

INSERT INTO focus_settings_new
    (persona_id, focus_id, context_flow, library_visibility,
     privacy_tier, max_permitted_tier, focus_profile, voice_override,
     created_at, updated_at)
SELECT
    persona_id, focus_id, context_flow, library_visibility,
    privacy_tier,
    CASE max_permitted_tier
        WHEN 1 THEN 'local_only'
        WHEN 2 THEN 'anonymous_required'
        WHEN 3 THEN 'unrestricted'
    END,
    focus_profile, voice_override, created_at, updated_at
FROM focus_settings;

DROP TABLE focus_settings;

ALTER TABLE focus_settings_new RENAME TO focus_settings;

CREATE INDEX IF NOT EXISTS idx_focus_settings_focus_id
    ON focus_settings (focus_id);

-- ---------------------------------------------------------------------------
-- focus_settings_friction_decisions.{requested,existing}_max_permitted_tier:
-- INTEGER -> TEXT
-- ---------------------------------------------------------------------------

CREATE TABLE focus_settings_friction_decisions_new (
    id                            TEXT    PRIMARY KEY,
    persona_id                    TEXT    NOT NULL REFERENCES personas(id) ON DELETE CASCADE,
    focus_id                      TEXT    NOT NULL,
    decision                      TEXT    NOT NULL CHECK (decision IN ('proceed', 'cancel')),
    requested_privacy_tier        INTEGER CHECK (requested_privacy_tier IS NULL
                                                  OR requested_privacy_tier BETWEEN 1 AND 3),
    requested_focus_profile       TEXT    CHECK (requested_focus_profile IS NULL
                                                  OR requested_focus_profile IN
                                                      ('open', 'organized', 'protected')),
    requested_max_permitted_tier  TEXT    CHECK (requested_max_permitted_tier IS NULL
                                                  OR requested_max_permitted_tier IN (
                                                      'local_only', 'anonymous_required',
                                                      'anonymous_preferred', 'unrestricted'
                                                  )),
    -- Privacy tier / focus_profile / max_permitted_tier at the moment the
    -- gate fired -- preserved so an audit reader can see what the user
    -- moved away from, not only what they moved to.
    existing_privacy_tier         INTEGER NOT NULL CHECK (existing_privacy_tier BETWEEN 1 AND 3),
    existing_focus_profile        TEXT    NOT NULL CHECK (existing_focus_profile IN
                                                  ('open', 'organized', 'protected')),
    existing_max_permitted_tier   TEXT    CHECK (existing_max_permitted_tier IS NULL
                                                  OR existing_max_permitted_tier IN (
                                                      'local_only', 'anonymous_required',
                                                      'anonymous_preferred', 'unrestricted'
                                                  )),
    created_at                    TEXT    NOT NULL,

    -- At least one of the three requested_* fields must be present -- a row
    -- with none would mean the gate fired for no reason.
    CHECK (requested_privacy_tier IS NOT NULL
           OR requested_focus_profile IS NOT NULL
           OR requested_max_permitted_tier IS NOT NULL)
);

INSERT INTO focus_settings_friction_decisions_new
    (id, persona_id, focus_id, decision, requested_privacy_tier,
     requested_focus_profile, requested_max_permitted_tier,
     existing_privacy_tier, existing_focus_profile, existing_max_permitted_tier,
     created_at)
SELECT
    id, persona_id, focus_id, decision, requested_privacy_tier,
    requested_focus_profile,
    CASE requested_max_permitted_tier
        WHEN 1 THEN 'local_only'
        WHEN 2 THEN 'anonymous_required'
        WHEN 3 THEN 'unrestricted'
        ELSE NULL
    END,
    existing_privacy_tier, existing_focus_profile,
    CASE existing_max_permitted_tier
        WHEN 1 THEN 'local_only'
        WHEN 2 THEN 'anonymous_required'
        WHEN 3 THEN 'unrestricted'
        ELSE NULL
    END,
    created_at
FROM focus_settings_friction_decisions;

DROP TABLE focus_settings_friction_decisions;

ALTER TABLE focus_settings_friction_decisions_new
    RENAME TO focus_settings_friction_decisions;

CREATE INDEX IF NOT EXISTS idx_focus_settings_friction_decisions_lookup
    ON focus_settings_friction_decisions (persona_id, focus_id, created_at DESC);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (20, datetime('now'),
    'items.id=529: focus_settings.max_permitted_tier and focus_settings_friction_decisions.{requested,existing}_max_permitted_tier rebuilt INTEGER -> TEXT (ExternalAccess::as_str() values), giving AnonymousPreferred a real authorable/storable slot');
