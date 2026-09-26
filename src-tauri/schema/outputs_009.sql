-- outputs_009.sql
--
-- items.id=574 Part 1 (shared foundation) -- focus_run_steps: the
-- per-step, multi-run ledger backing (a) the Active Board's per-run
-- pending/running/complete/failed surfacing, (b) reenter_step()'s
-- non-linear navigation, and (c) scheduled steps (a NEW, one-shot
-- StepTrigger, distinct from HighPriorityTrigger). (b) and (c) are
-- FOLLOW-UP dispatches, not built here -- see this item's own record for
-- the full design. This migration is schema only: no application code
-- writes or reads this table yet.
--
-- REVISION NOTE (this file replaces a first draft Jason reviewed and
-- rejected): the draft used an explicit self-referential `superseded_by`
-- pointer (outputs.superseded_by's own shape) and a 4-value status enum
-- that conflated "this attempt's own outcome was failure" with "this
-- attempt was retired because a newer one replaced it". Both are wrong for
-- this table specifically:
--   - superseded_by is REDUNDANT here in a way it is NOT for outputs.
--     outputs rows are independent documents (UUIDs with no inherent
--     ordering), so knowing WHICH row replaced a stale one genuinely
--     requires a stored pointer. focus_run_steps rows are NOT independent
--     -- they are already keyed by (focus_run_id, step_id, attempt), and
--     attempt is a plain monotonically increasing integer per step. "the
--     row that superseded this one" is therefore always exactly the row
--     with attempt+1 for the same (focus_run_id, step_id) -- a one-line
--     query against the natural key, not a fact that needs its own column.
--     Carrying superseded_by anyway would mean two mechanisms (the pointer
--     AND the attempt sequence) encoding the same relationship, with no
--     guarantee they'd ever disagree -- exactly the kind of redundant
--     state this schema should avoid, not import from a table (outputs)
--     that has a real reason for it.
--   - the fix is a 5-value status (pending/running/complete/failed/
--     discarded) instead of 4: 'failed' is a real, LIVE outcome (the
--     current attempt's own execution failed and nothing has retried it
--     yet); 'discarded' is a distinct, non-outcome meta-state meaning "this
--     row is no longer the live attempt for this step, superseded by a
--     later attempt" -- applied to whatever status the row held (complete,
--     failed, or an interrupted running/pending) the moment reenter_step()
--     retires it. Conflating the two into one 'discarded' value (the
--     draft's mistake) made it impossible to tell "this step's last real
--     attempt failed and nothing has happened since" from "this attempt
--     was intentionally retired" -- genuinely different facts for an audit
--     view to show.
--
-- ATTEMPT-VERSIONED, MIRRORING outputs' OWN PROVEN PATTERN in spirit
-- (outputs_007.sql, decisions.id=421/422) but not mechanically: one row per
-- (focus_run_id, step_id) ATTEMPT, never mutated into a different attempt
-- in place. Re-entering a step (the follow-up dispatch's reenter_step())
-- sets the current live row's status to 'discarded', then inserts a fresh
-- row with attempt+1. One table, no second history table to drift out of
-- sync with it -- same "no sync-drift risk" reasoning this item's record
-- already states, achieved here via the attempt sequence instead of a
-- pointer column.
--
-- sequence_index: this attempt's position in focus_def.steps at the time
-- the row was created (NOT the same thing as `attempt`, which counts
-- revisits of one step_id, not step order). Needed by reenter_step() to
-- reason about "everything before/after this point" without re-parsing
-- the .focus file, and by any ordered per-run history view.
--
-- content / sensitivity_severity / routing_tier_used: TaskStep's own three
-- data fields (types.rs), carried directly on the ledger row rather than
-- requiring a join through output_id -- most steps never produce a
-- Library-saved output at all (output_id stays NULL for those), so if this
-- table only pointed at outputs, most rows would have no recorded content.
-- Nullable -- unset until the step actually runs (status='pending').
--
-- output_id: REVERSE FK from step to output, not a new column on outputs
-- pointing the other way -- outputs.focus_run_id already identifies which
-- run an output belongs to, and adding a second, opposite-direction
-- pointer (outputs.step_id) would create a two-way-pointing FK hazard
-- (which side is authoritative on conflict) for no query this item's
-- design needs. Nullable -- only the subset of steps whose content became
-- a real Library output ever set it.
--
-- scheduled_for: nullable RFC3339 timestamp, set only for a step carrying
-- a one-shot StepTrigger (schedule_trigger, this item's record). NULL for
-- every ordinary inline_composable step. The follow-up scheduled-sweep
-- dispatch owns deciding its own query shape (and, if it needs one, its
-- own supporting index) once that step type actually exists -- adding an
-- index against a column nothing yet writes would be guessing at a query
-- this migration has no way to get right.
--
-- status: 'pending'/'running'/'complete'/'failed' are a step's real
-- execution outcomes (Active Board surfacing); 'discarded' is the retired-
-- attempt meta-state described above. Distinct from outputs.status's four
-- document-lifecycle states -- this is per-step run progress, not document
-- lifecycle -- so it gets its own vocabulary rather than reusing outputs'
-- CHECK list.
--
-- RETENTION (a real gap in the original design, not just this draft --
-- focus_run_snapshots, this table's sibling, already has one: a purge_after
-- column with status-dependent rules, outputs_001.sql, swept on startup).
-- focus_run_steps needs the equivalent, but NOT the same mechanism:
-- focus_run_snapshots' checkpoint cadence is SYSTEM-driven and bounded by
-- QR_CHECKPOINT_EVERY_N_STEPS -- an age-based purge_after works there
-- because checkpoint volume over any given time window is inherently
-- capped by that same constant. focus_run_steps' 'discarded' rows are
-- USER-driven -- created only when a person actually re-enters a step --
-- and that cadence has no system-imposed bound: a person iterating rapidly
-- on one step could produce hundreds of discarded attempts in an afternoon,
-- all well inside any age-based retention window, so a purge_after column
-- would not actually cap the growth this item's review flagged. A per-
-- (focus_run_id, step_id) COUNT cap does, directly: keep the most recent N
-- discarded attempts for a given step and drop the rest, so worst-case
-- table size becomes O(steps_per_run x N) regardless of how many times any
-- one step gets revisited, which is the exact unbounded-growth vector
-- described. This also needs no purge_after column or startup sweep at
-- all -- unlike focus_run_snapshots' deferred/scheduled purge, the cap is
-- enforced SYNCHRONOUSLY at the same point growth happens (reenter_step()
-- retires the old live row and trims that step_id's 'discarded' history to
-- N in the same transaction), so there is no backlog window where an
-- un-swept excess can accumulate. 'complete'/'failed'/'pending'/'running'
-- rows never need capping in the first place -- idx_focus_run_steps_live
-- below already guarantees at most one of those exists per step at a time;
-- only 'discarded' rows accumulate. The actual cap enforcement is
-- reenter_step()'s own application-layer logic (follow-up dispatch, not
-- built here, matching this migration's schema-only scope) -- suggested
-- default N=10 via an env var following this file's own
-- QR_CHECKPOINT_EVERY_N_STEPS precedent (e.g.
-- QR_FOCUS_RUN_STEPS_RETAIN_PER_STEP) rather than a hardcoded constant.
-- idx_focus_run_steps_retention below is the index that enforcement needs
-- to run as an indexed top-N-by-attempt scan instead of a full table scan.
--
-- FOUR INDEXES:
--   idx_focus_run_steps_live: UNIQUE, partial (status != 'discarded') --
--     this IS the "live state" query this item's record describes ("live
--     state is a partial-unique-index query, history is the same table
--     unfiltered including discarded rows"): at most one non-discarded
--     attempt may exist per (focus_run_id, step_id) at a time, whatever
--     its own status (pending/running/complete/failed) — and reading
--     WHERE status != 'discarded' for a run returns exactly its current
--     per-step state. The UNIQUE-ness is a real data-integrity guard, not
--     just a lookup accelerator -- it makes "two live attempts of the same
--     step in the same run" a constraint violation, not an application bug
--     that could silently corrupt Active Board's surfacing.
--   idx_focus_run_steps_run: history/dashboard scan, mirrors
--     idx_outputs_focus_run's (focus_run_id, created_at DESC) shape.
--   idx_focus_run_steps_output: mirrors idx_outputs_superseded_by/
--     idx_outputs_parent's partial-index-on-nullable-FK shape, for the
--     reverse output_id lookup described above.
--   idx_focus_run_steps_retention: (focus_run_id, step_id, attempt DESC) --
--     supports both the per-step_id retention cap above (an indexed
--     top-N-by-attempt scan instead of a full table scan) and a per-step
--     "what changed" audit view (a single step_id's full attempt history,
--     most recent first).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS focus_run_steps (
    id                      TEXT PRIMARY KEY,
    focus_run_id            TEXT NOT NULL REFERENCES focus_runs(id) ON DELETE CASCADE,
    step_id                 TEXT NOT NULL,
    attempt                 INTEGER NOT NULL DEFAULT 1,
    sequence_index          INTEGER NOT NULL,
    status                  TEXT NOT NULL DEFAULT 'pending'
                                CHECK (status IN
                                    ('pending','running','complete','failed','discarded')),
    content                 TEXT,
    sensitivity_severity    INTEGER,
    routing_tier_used       INTEGER,
    output_id               TEXT REFERENCES outputs(id) ON DELETE SET NULL,
    scheduled_for           TEXT,
    started_at              TEXT,
    completed_at            TEXT,
    created_at              TEXT NOT NULL,
    updated_at              TEXT NOT NULL,
    extra_metadata          TEXT NOT NULL DEFAULT '{}'
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_focus_run_steps_live
    ON focus_run_steps (focus_run_id, step_id)
    WHERE status != 'discarded';

CREATE INDEX IF NOT EXISTS idx_focus_run_steps_run
    ON focus_run_steps (focus_run_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_focus_run_steps_output
    ON focus_run_steps (output_id)
    WHERE output_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_focus_run_steps_retention
    ON focus_run_steps (focus_run_id, step_id, attempt DESC);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (9, datetime('now'),
    'items.id=574 (shared foundation, revised): focus_run_steps -- attempt-versioned per-step ledger for Active Board surfacing, reenter_step() navigation, and scheduled steps; 5-value status (pending/running/complete/failed/discarded) replaces a superseded_by pointer since attempt already encodes ordering; per-step_id retention cap replaces an age-based purge_after since growth here is user-revisit-driven, not system-cadence-driven');
