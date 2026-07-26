-- shared_002.sql
--
-- items.id=92 -- friction gate enforcement for update_focus_settings.
-- Adds focus_settings_friction_decisions: an audit-grade record of every
-- user decision to override the friction gate (a settings change that
-- loosens privacy_tier or moves focus_profile to 'protected').
--
-- WHY A NEW TABLE, NOT consent_decisions (outputs.db):
-- consent_decisions (outputs_001.sql) is the established pattern for
-- exactly this kind of decision -- push event, dedicated submit_* command,
-- durable append-only record -- and was the first option considered here.
-- It was rejected on a structural, not administrative, basis:
-- consent_decisions.focus_run_id is NOT NULL REFERENCES focus_runs(id).
-- Every existing consent_decisions row (gate3, floor) is written DURING a
-- FocusRun's execution, where a real run exists to anchor to. A friction
-- gate on update_focus_settings fires from a settings screen -- there is no
-- FocusRun in progress and no focus_run_id to supply. Forcing a value in
-- (a sentinel run id, or loosening the column to nullable) would corrupt
-- that column's single clear invariant for gate3/floor as well as this new
-- type, not just accommodate a third kind of row. A sibling table keyed by
-- what is actually being consented to -- (persona_id, focus_id), matching
-- focus_settings itself -- keeps the audit trail equally durable and
-- equally discoverable without weakening an existing table's contract.
--
-- decision values mirror 'floor' consent's asymmetric-by-type convention
-- (outputs_001.sql: "decision values are intentionally asymmetric --
-- different UX semantics. Do not normalize these values across types."):
--   'proceed' | 'cancel'
--
-- requested_privacy_tier / requested_focus_profile: the values the user's
-- update_focus_settings call was attempting to set, captured at decision
-- time. Nullable because a given friction-gate trip may be about tier only,
-- profile only, or both -- mirrors how update_focus_settings itself treats
-- every field as independently optional.
--
-- Append-only design, same rationale as consent_decisions: a persona/focus
-- pair may accumulate multiple decisions over time as settings are revisited.
-- No UNIQUE constraint on (persona_id, focus_id) -- multiple rows permitted.
-- Readers MUST order by created_at DESC and select the newest row for "what
-- did the user last decide" queries.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS focus_settings_friction_decisions (
    id                        TEXT    PRIMARY KEY,
    persona_id                TEXT    NOT NULL REFERENCES personas(id) ON DELETE CASCADE,
    focus_id                  TEXT    NOT NULL,
    decision                  TEXT    NOT NULL CHECK (decision IN ('proceed', 'cancel')),
    requested_privacy_tier    INTEGER CHECK (requested_privacy_tier IS NULL
                                              OR requested_privacy_tier BETWEEN 1 AND 3),
    requested_focus_profile   TEXT    CHECK (requested_focus_profile IS NULL
                                              OR requested_focus_profile IN
                                                  ('open', 'organized', 'protected')),
    -- Privacy tier and/or focus_profile at the moment the gate fired --
    -- preserved so an audit reader can see what the user moved away from,
    -- not only what they moved to. Mirrors the pair reasoning above.
    existing_privacy_tier     INTEGER NOT NULL CHECK (existing_privacy_tier BETWEEN 1 AND 3),
    existing_focus_profile    TEXT    NOT NULL CHECK (existing_focus_profile IN
                                                  ('open', 'organized', 'protected')),
    created_at                TEXT    NOT NULL,

    -- At least one of the two requested_* fields must be present -- a row
    -- with neither would mean the gate fired for no reason.
    CHECK (requested_privacy_tier IS NOT NULL OR requested_focus_profile IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS idx_focus_settings_friction_decisions_lookup
    ON focus_settings_friction_decisions (persona_id, focus_id, created_at DESC);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (2, datetime('now'),
    'items.id=92: focus_settings_friction_decisions -- audit record for friction gate overrides on update_focus_settings');
